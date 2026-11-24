//! `GcsBlobStore` against the shared blob store contract over real loopback HTTP, plus an
//! env-gated run against any GCS-compatible endpoint (real GCS or `fake-gcs-server`).

mod support {
    pub mod blob_contract;
    pub mod fake_gcs;
}

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use open_ci_cache::{
    BlobId, BlobStore, BlobUpload, BlockId, ByteRange, CacheError, GcsBlobStore, GcsStoreConfig,
};
use open_ci_core::SecretString;
use open_ci_provider_gcp::StaticTokenSource;
use std::sync::Arc;
use support::blob_contract::{TestResult, assert_blob_store_contract};
use support::fake_gcs::{FakeGcs, MAX_COMPOSE_SOURCES};

const PREFIX: &str = "/ci/cache/";
const MANY_BLOCKS: usize = 70;

fn store(
    endpoint: &str,
    bucket: &str,
    prefix: &str,
    token: &str,
) -> Result<GcsBlobStore, CacheError> {
    let config = GcsStoreConfig {
        endpoint: endpoint.to_owned(),
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
    };
    let tokens = Arc::new(StaticTokenSource(SecretString::new(token)));
    GcsBlobStore::new(config, tokens, reqwest::Client::new())
}

fn fake_store(fake: &FakeGcs) -> Result<GcsBlobStore, CacheError> {
    store(&fake.endpoint, &fake.bucket, PREFIX, &fake.token)
}

fn block_bytes(index: usize) -> Bytes {
    (0..=index * 37 % 101)
        .map(|offset| u8::try_from((index * 7 + offset) % 256).unwrap_or(0))
        .collect::<Vec<u8>>()
        .into()
}

async fn read_whole(
    store: &dyn BlobStore,
    blob: &BlobId,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let size = store.size(blob).await?.ok_or("blob missing")?;
    let range = ByteRange::full(size).ok_or("blob empty")?;
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

async fn assert_many_blocks_join_exactly(store: &dyn BlobStore, blob: &BlobId) -> TestResult {
    let ids: Vec<BlockId> = (0..MANY_BLOCKS)
        .map(|i| BlockId::parse(&format!("block-{i:05}")))
        .collect::<Result<_, _>>()?;
    for (i, id) in ids.iter().enumerate().rev() {
        store
            .stage_block(blob, id, BlobUpload::from_bytes(block_bytes(i)))
            .await?;
    }
    let expected: Bytes = (0..MANY_BLOCKS)
        .map(block_bytes)
        .collect::<Vec<_>>()
        .concat()
        .into();
    let size = store.commit_blocks(blob, &ids, u64::MAX).await?;
    assert_eq!(size, expected.len() as u64);
    assert_eq!(read_whole(store, blob).await?, expected);
    Ok(())
}

async fn assert_zero_byte_join_publishes_empty_blob(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let ids = [BlockId::parse("empty-a")?, BlockId::parse("empty-b")?];
    for id in &ids {
        store
            .stage_block(&blob, id, BlobUpload::from_bytes(Bytes::new()))
            .await?;
    }
    assert_eq!(store.commit_blocks(&blob, &ids, 0).await?, 0);
    assert_eq!(store.size(&blob).await?, Some(0));
    store.delete(&blob).await?;
    Ok(())
}

#[tokio::test]
async fn gcs_blob_store_satisfies_contract_against_fake_gcs() -> TestResult {
    let fake = FakeGcs::start().await?;
    let store = fake_store(&fake)?;
    assert_blob_store_contract(&store).await?;
    assert_zero_byte_join_publishes_empty_blob(&store).await?;
    assert_eq!(
        fake.object_names()?,
        Vec::<String>::new(),
        "every object cleaned up"
    );
    Ok(())
}

#[tokio::test]
async fn commit_of_more_than_32_blocks_composes_in_levels_and_cleans_up() -> TestResult {
    let fake = FakeGcs::start().await?;
    let store = fake_store(&fake)?;
    let blob = BlobId::generate();
    assert_many_blocks_join_exactly(&store, &blob).await?;

    let intermediates = MANY_BLOCKS.div_ceil(MAX_COMPOSE_SOURCES);
    assert!(intermediates > 1 && intermediates <= MAX_COMPOSE_SOURCES);
    assert_eq!(fake.compose_calls(), intermediates + 1);
    assert_eq!(fake.object_names()?, vec![format!("ci/cache/blobs/{blob}")]);

    store.delete(&blob).await?;
    assert_eq!(fake.object_names()?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn delete_pages_through_every_staged_object() -> TestResult {
    let fake = FakeGcs::start().await?;
    let store = fake_store(&fake)?;
    let blob = BlobId::generate();
    for i in 0..10 {
        store
            .stage_block(
                &blob,
                &BlockId::parse(&format!("orphan-{i}"))?,
                BlobUpload::from_bytes(block_bytes(i)),
            )
            .await?;
    }
    assert_eq!(fake.object_names()?.len(), 10);
    store.delete(&blob).await?;
    assert_eq!(fake.object_names()?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn overlong_body_never_publishes_when_excess_arrives_after_declared_length() -> TestResult {
    let fake = FakeGcs::start().await?;
    let store = fake_store(&fake)?;
    let blob = BlobId::generate();
    let chunks: Vec<std::io::Result<Bytes>> =
        vec![Ok(block_bytes(3)), Ok(Bytes::from_static(b"!"))];
    let declared = block_bytes(3).len() as u64;
    let upload = BlobUpload::new(declared, futures_util::stream::iter(chunks).boxed());
    let result = store.put(&blob, upload).await;
    assert!(
        matches!(result, Err(CacheError::BadRequest(_))),
        "{result:?}"
    );
    assert_eq!(store.size(&blob).await?, None);
    assert_eq!(fake.object_names()?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn fake_gcs_enforces_compose_limit_and_bearer_token() -> TestResult {
    let fake = FakeGcs::start().await?;
    let sources: Vec<_> = (0..=MAX_COMPOSE_SOURCES)
        .map(|i| serde_json::json!({ "name": format!("src-{i}") }))
        .collect();
    let response = reqwest::Client::new()
        .post(format!(
            "{}/storage/v1/b/{}/o/dest/compose",
            fake.endpoint, fake.bucket
        ))
        .bearer_auth(&fake.token)
        .json(&serde_json::json!({
            "sourceObjects": sources,
            "destination": { "contentType": "application/octet-stream" },
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    let wrong_token = "wrong-secret-token";
    let rejected = store(&fake.endpoint, &fake.bucket, PREFIX, wrong_token)?
        .put(
            &BlobId::generate(),
            BlobUpload::from_bytes(Bytes::from_static(b"x")),
        )
        .await;
    let Err(CacheError::Storage(message)) = rejected else {
        return Err(format!("expected storage error, got {rejected:?}").into());
    };
    assert!(message.contains("401"), "{message}");
    assert!(
        !message.contains(wrong_token) && !message.contains(&fake.token),
        "{message}"
    );
    assert_eq!(fake.object_names()?, Vec::<String>::new());
    Ok(())
}

/// Runs the contract against `OPEN_CI_TEST_GCS_ENDPOINT` / `OPEN_CI_TEST_GCS_BUCKET` with
/// `OPEN_CI_TEST_GCS_TOKEN` (emulators accept any token), under a unique per-run prefix.
#[tokio::test]
async fn gcs_compatible_endpoint_satisfies_contract() -> TestResult {
    let (Ok(endpoint), Ok(bucket)) = (
        std::env::var("OPEN_CI_TEST_GCS_ENDPOINT"),
        std::env::var("OPEN_CI_TEST_GCS_BUCKET"),
    ) else {
        eprintln!(
            "skipping: set OPEN_CI_TEST_GCS_ENDPOINT and OPEN_CI_TEST_GCS_BUCKET to run against GCS"
        );
        return Ok(());
    };
    let token = std::env::var("OPEN_CI_TEST_GCS_TOKEN").unwrap_or_else(|_| "emulator".to_owned());
    let prefix = format!("open-ci-cache-test/{}", uuid::Uuid::new_v4());
    let store = store(&endpoint, &bucket, &prefix, &token)?;
    assert_blob_store_contract(&store).await?;
    assert_zero_byte_join_publishes_empty_blob(&store).await?;
    let blob = BlobId::generate();
    assert_many_blocks_join_exactly(&store, &blob).await?;
    store.delete(&blob).await?;
    assert_eq!(store.size(&blob).await?, None);
    Ok(())
}

#[tokio::test]
async fn published_blob_survives_failed_staging_cleanup() -> TestResult {
    let fake = FakeGcs::start().await?;
    let store = fake_store(&fake)?;
    let blob = BlobId::generate();
    let ids = [BlockId::parse("one")?, BlockId::parse("two")?];
    for (i, id) in ids.iter().enumerate() {
        store
            .stage_block(&blob, id, BlobUpload::from_bytes(block_bytes(i)))
            .await?;
    }

    fake.fail_deletes(true);
    let size = store.commit_blocks(&blob, &ids, u64::MAX).await?;
    assert_eq!(
        read_whole(&store, &blob).await?,
        [block_bytes(0), block_bytes(1)].concat()
    );
    assert_eq!(size, (block_bytes(0).len() + block_bytes(1).len()) as u64);

    fake.fail_deletes(false);
    store.delete(&blob).await?;
    assert_eq!(
        fake.object_names()?,
        Vec::<String>::new(),
        "a later delete removes leftovers"
    );
    Ok(())
}
