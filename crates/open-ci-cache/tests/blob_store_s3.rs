//! `S3BlobStore` against the shared blob store contract over a loopback fake S3, S3-specific
//! wire behavior, and an opt-in run against a real S3-compatible service.

mod support {
    pub mod blob_contract;
    pub mod fake_s3;
}

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use open_ci_cache::{
    BlobId, BlobStore, BlobUpload, BlockId, ByteRange, CacheError, S3BlobStore, S3StoreConfig,
};
use open_ci_core::SecretString;
use open_ci_provider_aws::sigv4::AwsCredentials;
use open_ci_provider_aws::{AwsCredentialsSource, StaticAwsCredentials};
use std::sync::Arc;
use support::blob_contract::{TestResult, assert_blob_store_contract};
use support::fake_s3::{FakeS3, FakeS3Options};

const ACCESS_KEY_ID: &str = "AKIAOPENCIFAKE";
const SECRET_ACCESS_KEY: &str = "open-ci-fake-secret-access-key";
const REGION: &str = "us-west-2";
const BUCKET: &str = "open-ci-cache";
const MIB: usize = 1024 * 1024;

fn fake_options(session_token: Option<&str>) -> FakeS3Options {
    FakeS3Options {
        access_key_id: ACCESS_KEY_ID.to_owned(),
        region: REGION.to_owned(),
        bucket: BUCKET.to_owned(),
        session_token: session_token.map(str::to_owned),
        list_page_size: 2,
    }
}

fn credentials(access_key_id: &str, session_token: Option<&str>) -> Arc<dyn AwsCredentialsSource> {
    Arc::new(StaticAwsCredentials(AwsCredentials {
        access_key_id: access_key_id.to_owned(),
        secret_access_key: SecretString::new(SECRET_ACCESS_KEY),
        session_token: session_token.map(SecretString::new),
    }))
}

fn store_for(
    fake: &FakeS3,
    prefix: &str,
    credentials: Arc<dyn AwsCredentialsSource>,
) -> Result<S3BlobStore, CacheError> {
    S3BlobStore::new(
        S3StoreConfig {
            endpoint: fake.endpoint().to_owned(),
            region: REGION.to_owned(),
            bucket: BUCKET.to_owned(),
            prefix: prefix.to_owned(),
        },
        credentials,
        reqwest::Client::new(),
    )
}

fn pattern(len: usize, seed: u8) -> Bytes {
    (0..len)
        .map(|i| seed.wrapping_add(u8::try_from(i % 251).unwrap_or(0)))
        .collect::<Vec<u8>>()
        .into()
}

async fn read_all(
    store: &dyn BlobStore,
    blob: &BlobId,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let size = store.size(blob).await?.ok_or("blob missing")?;
    let range = ByteRange::full(size).ok_or("blob empty")?;
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

#[tokio::test]
async fn contract_holds_against_loopback_s3_and_leaves_no_objects_behind() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "/ci cache+v1/", credentials(ACCESS_KEY_ID, None))?;

    assert_blob_store_contract(&store).await?;

    let operations = fake.take_operations().await;
    for expected in [
        "PutObject",
        "UploadPartCopy",
        "UploadPart",
        "CompleteMultipartUpload",
        "ListObjectsV2",
    ] {
        assert!(operations.contains(&expected), "{expected} never issued");
    }
    assert_eq!(fake.object_keys().await, Vec::<String>::new());
    assert_eq!(fake.open_uploads().await, 0);

    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(pattern(3, 1)))
        .await?;
    assert_eq!(
        fake.object_keys().await,
        [format!("ci cache+v1/blobs/{blob}")]
    );
    Ok(())
}

#[tokio::test]
async fn commit_copies_full_blocks_server_side_and_streams_runs_of_small_blocks() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "", credentials(ACCESS_KEY_ID, None))?;
    let blob = BlobId::generate();
    let parts = [
        pattern(6 * MIB, 1),
        pattern(2 * MIB, 2),
        pattern(4 * MIB, 3),
        pattern(1024, 4),
    ];
    let mut ids = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let id = BlockId::parse(&format!("block-{i}"))?;
        store
            .stage_block(&blob, &id, BlobUpload::from_bytes(part.clone()))
            .await?;
        ids.push(id);
    }
    for extra in ["unused-a", "unused-b", "unused-c"] {
        store
            .stage_block(
                &blob,
                &BlockId::parse(extra)?,
                BlobUpload::from_bytes(pattern(5, 9)),
            )
            .await?;
    }
    fake.take_operations().await;

    let size = store.commit_blocks(&blob, &ids, u64::MAX).await?;

    let expected: Bytes = parts.concat().into();
    assert_eq!(size, expected.len() as u64);
    let operations = fake.take_operations().await;
    let count = |name: &str| operations.iter().filter(|op| **op == name).count();
    assert_eq!(count("UploadPartCopy"), 2, "{operations:?}");
    assert_eq!(count("UploadPart"), 1, "{operations:?}");
    assert_eq!(count("CompleteMultipartUpload"), 1, "{operations:?}");
    assert_eq!(count("GetObject"), 2, "{operations:?}");
    assert!(count("ListObjectsV2") >= 2, "pagination never exercised");
    assert_eq!(fake.object_keys().await, [format!("blobs/{blob}")]);
    assert_eq!(read_all(&store, &blob).await?, expected);
    Ok(())
}

#[tokio::test]
async fn ok_status_error_documents_fail_the_commit_and_abort_the_upload() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "", credentials(ACCESS_KEY_ID, None))?;
    for operation in ["UploadPartCopy", "CompleteMultipartUpload"] {
        let blob = BlobId::generate();
        let ids = [BlockId::parse("big")?, BlockId::parse("tail")?];
        store
            .stage_block(&blob, &ids[0], BlobUpload::from_bytes(pattern(6 * MIB, 1)))
            .await?;
        store
            .stage_block(&blob, &ids[1], BlobUpload::from_bytes(pattern(10, 2)))
            .await?;
        fake.fail_next_with_ok_status(operation).await;
        fake.take_operations().await;

        let result = store.commit_blocks(&blob, &ids, u64::MAX).await;

        assert!(
            matches!(&result, Err(CacheError::Storage(msg)) if msg.contains("InternalError")),
            "{operation}: {result:?}"
        );
        assert_eq!(store.size(&blob).await?, None, "{operation}");
        assert_eq!(fake.open_uploads().await, 0, "{operation}");
        assert!(
            fake.take_operations()
                .await
                .contains(&"AbortMultipartUpload"),
            "{operation}"
        );

        assert_eq!(
            store.commit_blocks(&blob, &ids, u64::MAX).await?,
            6 * MIB as u64 + 10
        );
        store.delete(&blob).await?;
    }
    assert_eq!(fake.object_keys().await, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn session_tokens_are_sent_and_signed() -> TestResult {
    let token = "session/token+with=reserved";
    let fake = FakeS3::start(fake_options(Some(token))).await?;
    let store = store_for(&fake, "", credentials(ACCESS_KEY_ID, Some(token)))?;
    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(pattern(100, 3)))
        .await?;
    assert_eq!(read_all(&store, &blob).await?, pattern(100, 3));

    let without_token = store_for(&fake, "", credentials(ACCESS_KEY_ID, None))?;
    let result = without_token.size(&blob).await;
    assert!(
        matches!(&result, Err(CacheError::Storage(msg)) if msg.contains("403")),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn rejected_requests_report_status_without_credentials() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "", credentials("AKIAUNKNOWN", None))?;
    let result = store
        .put(&BlobId::generate(), BlobUpload::from_bytes(pattern(10, 1)))
        .await;
    let Err(CacheError::Storage(message)) = result else {
        return Err(format!("expected storage error, got {result:?}").into());
    };
    assert!(
        message.contains("PutObject") && message.contains("403"),
        "{message}"
    );
    for secret in [
        SECRET_ACCESS_KEY,
        "Signature=",
        "Credential=",
        "AKIAUNKNOWN",
    ] {
        assert!(!message.contains(secret), "{message}");
    }
    Ok(())
}

#[tokio::test]
async fn client_body_length_mismatch_is_a_bad_request() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "", credentials(ACCESS_KEY_ID, None))?;
    let data = pattern(1000, 1);
    for declared in [999, 1001] {
        let chunk = data.clone();
        let upload = BlobUpload::new(declared, stream::once(async move { Ok(chunk) }).boxed());
        let result = store.put(&BlobId::generate(), upload).await;
        assert!(
            matches!(result, Err(CacheError::BadRequest(_))),
            "{declared}: {result:?}"
        );
    }
    assert_eq!(fake.object_keys().await, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn bodies_over_s3_limits_are_rejected_before_any_request() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    let store = store_for(&fake, "", credentials(ACCESS_KEY_ID, None))?;
    let blob = BlobId::generate();
    let oversized = |len: u64| BlobUpload::new(len, stream::empty().boxed());

    let put = store
        .put(&blob, oversized(5 * 1024 * 1024 * 1024 + 1))
        .await;
    assert!(
        matches!(put, Err(CacheError::PayloadTooLarge(_))),
        "{put:?}"
    );
    let staged = store
        .stage_block(
            &blob,
            &BlockId::parse("huge")?,
            oversized(4000 * 1024 * 1024 + 1),
        )
        .await;
    assert!(
        matches!(staged, Err(CacheError::PayloadTooLarge(_))),
        "{staged:?}"
    );
    assert_eq!(fake.take_operations().await, Vec::<&str>::new());
    Ok(())
}

#[tokio::test]
async fn invalid_store_config_is_rejected() -> TestResult {
    let fake = FakeS3::start(fake_options(None)).await?;
    for (endpoint, bucket, prefix) in [
        ("not a url", BUCKET, ""),
        (fake.endpoint(), "Bad_Bucket", ""),
        (fake.endpoint(), BUCKET, "a/../b"),
    ] {
        let result = S3BlobStore::new(
            S3StoreConfig {
                endpoint: endpoint.to_owned(),
                region: REGION.to_owned(),
                bucket: bucket.to_owned(),
                prefix: prefix.to_owned(),
            },
            credentials(ACCESS_KEY_ID, None),
            reqwest::Client::new(),
        );
        assert!(
            matches!(result, Err(CacheError::InvalidConfig(_))),
            "{endpoint} {bucket} {prefix}"
        );
    }
    Ok(())
}

/// Runs the contract against a real S3-compatible service when `OPEN_CI_TEST_S3_*` is set.
#[tokio::test]
async fn contract_holds_against_real_s3_compatible_service() -> TestResult {
    let var = |name: &str| std::env::var(format!("OPEN_CI_TEST_S3_{name}")).ok();
    let (Some(endpoint), Some(region), Some(bucket), Some(access_key_id), Some(secret)) = (
        var("ENDPOINT"),
        var("REGION"),
        var("BUCKET"),
        var("ACCESS_KEY_ID"),
        var("SECRET_ACCESS_KEY"),
    ) else {
        eprintln!(
            "skipping real S3 contract: OPEN_CI_TEST_S3_{{ENDPOINT,REGION,BUCKET,ACCESS_KEY_ID,SECRET_ACCESS_KEY}} unset"
        );
        return Ok(());
    };
    let credentials: Arc<dyn AwsCredentialsSource> =
        Arc::new(StaticAwsCredentials(AwsCredentials {
            access_key_id,
            secret_access_key: SecretString::new(secret),
            session_token: var("SESSION_TOKEN").map(SecretString::new),
        }));
    let store = S3BlobStore::new(
        S3StoreConfig {
            endpoint,
            region,
            bucket,
            prefix: format!("open-ci contract/{}+run/", BlobId::generate()),
        },
        credentials,
        reqwest::Client::new(),
    )?;
    assert_blob_store_contract(&store).await
}
