//! Behavioral contract every `BlobStore` backend must satisfy, run against each implementation.

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use open_ci_cache::{BlobId, BlobStore, BlobUpload, BlockId, ByteRange, CacheError};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

const MIB: usize = 1024 * 1024;

pub async fn assert_blob_store_contract(store: &dyn BlobStore) -> TestResult {
    single_put_roundtrips_full_and_ranged_reads(store).await?;
    empty_put_reports_zero_size(store).await?;
    put_replaces_existing_content(store).await?;
    blocks_join_in_commit_order_not_upload_order(store).await?;
    restaged_block_uses_latest_content(store).await?;
    large_and_small_blocks_join_exactly(store).await?;
    commit_rejects_missing_block_without_publishing(store).await?;
    commit_rejects_oversized_join_without_publishing(store).await?;
    short_and_long_bodies_fail_without_publishing(store).await?;
    delete_removes_blob_and_staged_blocks(store).await?;
    missing_blob_reads_as_not_found(store).await?;
    Ok(())
}

fn pattern(len: usize, seed: u8) -> Bytes {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|b| seed.wrapping_add(b))
        .collect::<Vec<u8>>()
        .into()
}

fn chunked(data: &Bytes, chunk: usize) -> BlobUpload {
    let len = data.len() as u64;
    let pieces: Vec<std::io::Result<Bytes>> = data
        .chunks(chunk.max(1))
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    BlobUpload::new(len, stream::iter(pieces).boxed())
}

async fn read_all(
    store: &dyn BlobStore,
    blob: &BlobId,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let size = store.size(blob).await?.ok_or("blob missing")?;
    let range = ByteRange::full(size).ok_or("blob empty")?;
    read_range(store, blob, range).await
}

async fn read_range(
    store: &dyn BlobStore,
    blob: &BlobId,
    range: ByteRange,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

fn block(name: &str) -> Result<BlockId, CacheError> {
    BlockId::parse(name)
}

async fn single_put_roundtrips_full_and_ranged_reads(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let data = pattern(3 * MIB + 17, 1);
    store.put(&blob, chunked(&data, 64 * 1024)).await?;

    assert_eq!(store.size(&blob).await?, Some(data.len() as u64));
    assert_eq!(read_all(store, &blob).await?, data);
    for header in [
        "bytes=0-0",
        "bytes=1000-1999",
        "bytes=-17",
        "bytes=3145728-",
    ] {
        let range = ByteRange::parse_header(header, data.len() as u64)?;
        let start = usize::try_from(range.start())?;
        let end = usize::try_from(range.end_inclusive())?;
        assert_eq!(
            read_range(store, &blob, range).await?,
            data.slice(start..=end),
            "range {header}"
        );
    }
    store.delete(&blob).await?;
    Ok(())
}

async fn empty_put_reports_zero_size(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(Bytes::new()))
        .await?;
    assert_eq!(store.size(&blob).await?, Some(0));
    store.delete(&blob).await?;
    Ok(())
}

async fn put_replaces_existing_content(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(pattern(100, 1)))
        .await?;
    store
        .put(&blob, BlobUpload::from_bytes(pattern(40, 9)))
        .await?;
    assert_eq!(read_all(store, &blob).await?, pattern(40, 9));
    store.delete(&blob).await?;
    Ok(())
}

async fn blocks_join_in_commit_order_not_upload_order(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let (a, b, c) = (pattern(1000, 1), pattern(2000, 2), pattern(500, 3));
    store
        .stage_block(&blob, &block("Yw==")?, BlobUpload::from_bytes(c.clone()))
        .await?;
    store
        .stage_block(&blob, &block("YQ==")?, BlobUpload::from_bytes(a.clone()))
        .await?;
    store
        .stage_block(&blob, &block("Yg==")?, BlobUpload::from_bytes(b.clone()))
        .await?;
    assert_eq!(
        store.size(&blob).await?,
        None,
        "staged blocks are not a blob"
    );

    let order = [block("YQ==")?, block("Yg==")?, block("Yw==")?];
    let size = store.commit_blocks(&blob, &order, u64::MAX).await?;
    assert_eq!(size, 3500);
    assert_eq!(read_all(store, &blob).await?, [a, b, c].concat());
    store.delete(&blob).await?;
    Ok(())
}

async fn restaged_block_uses_latest_content(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let id = block("MDAwMA==")?;
    store
        .stage_block(&blob, &id, BlobUpload::from_bytes(pattern(10, 1)))
        .await?;
    store
        .stage_block(&blob, &id, BlobUpload::from_bytes(pattern(20, 7)))
        .await?;
    store
        .commit_blocks(&blob, std::slice::from_ref(&id), u64::MAX)
        .await?;
    assert_eq!(read_all(store, &blob).await?, pattern(20, 7));
    store.delete(&blob).await?;
    Ok(())
}

async fn large_and_small_blocks_join_exactly(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let parts = [
        pattern(6 * MIB, 1),
        pattern(MIB, 2),
        pattern(MIB, 3),
        pattern(7 * MIB + 3, 4),
        pattern(10, 5),
    ];
    let mut ids = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let id = block(&format!("block-{i:04}"))?;
        store
            .stage_block(&blob, &id, chunked(part, 256 * 1024))
            .await?;
        ids.push(id);
    }
    let expected: Bytes = parts.concat().into();
    let size = store.commit_blocks(&blob, &ids, u64::MAX).await?;
    assert_eq!(size, expected.len() as u64);
    assert_eq!(read_all(store, &blob).await?, expected);
    store.delete(&blob).await?;
    Ok(())
}

async fn commit_rejects_missing_block_without_publishing(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    store
        .stage_block(
            &blob,
            &block("present")?,
            BlobUpload::from_bytes(pattern(10, 1)),
        )
        .await?;
    let result = store
        .commit_blocks(&blob, &[block("present")?, block("absent")?], u64::MAX)
        .await;
    assert!(
        matches!(result, Err(CacheError::BadRequest(_))),
        "{result:?}"
    );
    assert_eq!(store.size(&blob).await?, None);
    store.delete(&blob).await?;
    Ok(())
}

async fn commit_rejects_oversized_join_without_publishing(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    store
        .stage_block(
            &blob,
            &block("one")?,
            BlobUpload::from_bytes(pattern(60, 1)),
        )
        .await?;
    store
        .stage_block(
            &blob,
            &block("two")?,
            BlobUpload::from_bytes(pattern(60, 2)),
        )
        .await?;
    let result = store
        .commit_blocks(&blob, &[block("one")?, block("two")?], 100)
        .await;
    assert!(
        matches!(result, Err(CacheError::PayloadTooLarge(_))),
        "{result:?}"
    );
    assert_eq!(store.size(&blob).await?, None);
    store.delete(&blob).await?;
    Ok(())
}

async fn short_and_long_bodies_fail_without_publishing(store: &dyn BlobStore) -> TestResult {
    let data = pattern(1000, 1);
    let body = |declared: u64| {
        let chunk = data.clone();
        BlobUpload::new(declared, stream::once(async move { Ok(chunk) }).boxed())
    };
    for declared in [999, 1001] {
        let blob = BlobId::generate();
        assert!(
            store.put(&blob, body(declared)).await.is_err(),
            "declared {declared}"
        );
        assert_eq!(store.size(&blob).await?, None, "declared {declared}");
        store.delete(&blob).await?;
    }

    let blob = BlobId::generate();
    let exact_then_extra = vec![Ok(data.clone()), Ok(Bytes::from_static(b"!"))];
    let overrun = BlobUpload::new(1000, stream::iter(exact_then_extra).boxed());
    assert!(
        store.put(&blob, overrun).await.is_err(),
        "bytes after the declared length arrive in a later chunk"
    );
    assert_eq!(
        store.size(&blob).await?,
        None,
        "overrun body must not publish"
    );
    store.delete(&blob).await?;
    Ok(())
}

async fn delete_removes_blob_and_staged_blocks(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(pattern(10, 1)))
        .await?;
    store
        .stage_block(
            &blob,
            &block("left-over")?,
            BlobUpload::from_bytes(pattern(10, 2)),
        )
        .await?;
    store.delete(&blob).await?;
    store.delete(&blob).await?;
    assert_eq!(store.size(&blob).await?, None);
    let commit = store
        .commit_blocks(&blob, &[block("left-over")?], u64::MAX)
        .await;
    assert!(
        matches!(commit, Err(CacheError::BadRequest(_))),
        "{commit:?}"
    );
    Ok(())
}

async fn missing_blob_reads_as_not_found(store: &dyn BlobStore) -> TestResult {
    let blob = BlobId::generate();
    let range = ByteRange::full(10).ok_or("range")?;
    assert!(matches!(
        store.read(&blob, range).await,
        Err(CacheError::NotFound(_))
    ));
    Ok(())
}
