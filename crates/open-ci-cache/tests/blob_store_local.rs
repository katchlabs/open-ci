//! `LocalBlobStore` against the shared blob store contract, plus on-disk hygiene and manifests.

mod support {
    pub mod blob_contract;
}

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use open_ci_cache::{BlobId, BlobStore, BlobUpload, BlockId, ByteRange, LocalBlobStore};
use support::blob_contract::{TestResult, assert_blob_store_contract};
use tempfile::TempDir;

fn pattern(len: usize, seed: u8) -> Bytes {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|b| seed.wrapping_add(b))
        .collect::<Vec<u8>>()
        .into()
}

async fn read_range(
    store: &LocalBlobStore,
    blob: &BlobId,
    range: ByteRange,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

#[tokio::test]
async fn local_blob_store_satisfies_contract() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    assert_blob_store_contract(&store).await
}

#[tokio::test]
async fn local_blob_store_writes_private_files_and_leaves_no_temp_files() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    let blob = BlobId::generate();
    store
        .put(&blob, BlobUpload::from_bytes(Bytes::from_static(b"cache")))
        .await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir.path().join("blobs").join(blob.as_str()))?;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(std::fs::read_dir(dir.path().join("tmp"))?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn cancelled_upload_leaves_no_temp_file_and_reopen_clears_stragglers() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    let (body_stalled, stalled) = tokio::sync::oneshot::channel::<()>();
    let body = stream::once(async { Ok(Bytes::from_static(b"partial")) })
        .chain(stream::once(async move {
            let _ = body_stalled.send(());
            std::future::pending::<std::io::Result<Bytes>>().await
        }))
        .boxed();
    let blob = BlobId::generate();

    tokio::select! {
        result = store.put(&blob, BlobUpload::new(1_000, body)) => {
            return Err(format!("upload finished unexpectedly: {result:?}").into());
        }
        _ = stalled => {}
    }

    assert_eq!(std::fs::read_dir(dir.path().join("tmp"))?.count(), 0);
    assert_eq!(store.size(&blob).await?, None);

    std::fs::write(
        dir.path().join("tmp").join("crashed.tmp"),
        b"left by a crash",
    )?;
    LocalBlobStore::open(dir.path()).await?;
    assert_eq!(std::fs::read_dir(dir.path().join("tmp"))?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn multi_block_commit_uses_zero_copy_manifest_and_streams_partial_ranges() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    let blob = BlobId::generate();
    let b1 = BlockId::parse("block-1")?;
    let b2 = BlockId::parse("block-2")?;
    let b3 = BlockId::parse("block-3")?;
    let unused = BlockId::parse("unused")?;

    let d1 = pattern(1_000, 1);
    let d2 = pattern(2_000, 2);
    let d3 = pattern(1_500, 3);
    store
        .stage_block(&blob, &b1, BlobUpload::from_bytes(d1.clone()))
        .await?;
    store
        .stage_block(&blob, &b2, BlobUpload::from_bytes(d2.clone()))
        .await?;
    store
        .stage_block(&blob, &b3, BlobUpload::from_bytes(d3.clone()))
        .await?;
    store
        .stage_block(&blob, &unused, BlobUpload::from_bytes(pattern(500, 9)))
        .await?;

    let size = store
        .commit_blocks(&blob, &[b1.clone(), b2.clone(), b3.clone()], u64::MAX)
        .await?;
    assert_eq!(size, 4_500);
    assert_eq!(store.size(&blob).await?, Some(4_500));

    let blob_file = dir.path().join("blobs").join(blob.as_str());
    let manifest_file = dir
        .path()
        .join("blobs")
        .join(format!("{}.manifest", blob.as_str()));
    let committed_dir = dir.path().join("committed").join(blob.as_str());
    let staging_dir = dir.path().join("staging").join(blob.as_str());

    assert!(!blob_file.exists());
    assert!(manifest_file.exists());
    assert!(committed_dir.exists());
    assert!(!staging_dir.exists());
    assert_eq!(std::fs::read_dir(&committed_dir)?.count(), 3);
    assert!(!committed_dir.join(unused.storage_name()).exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&manifest_file)?;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    let expected: Bytes = [d1, d2, d3].concat().into();
    let full = ByteRange::full(4_500).ok_or("full range")?;
    assert_eq!(read_range(&store, &blob, full).await?, expected);

    for header in [
        "bytes=500-3499",
        "bytes=0-0",
        "bytes=999-1000",
        "bytes=2999-3000",
        "bytes=1000-1000",
        "bytes=4499-4499",
        "bytes=-2000",
    ] {
        let range = ByteRange::parse_header(header, 4_500)?;
        let start = usize::try_from(range.start())?;
        let end = usize::try_from(range.end_inclusive())?;
        assert_eq!(
            read_range(&store, &blob, range).await?,
            expected.slice(start..=end),
            "range {header}"
        );
    }

    store.delete(&blob).await?;
    assert!(!manifest_file.exists());
    assert!(!committed_dir.exists());
    assert_eq!(store.size(&blob).await?, None);
    Ok(())
}

#[tokio::test]
async fn repeated_block_ids_commit_and_read_correctly() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;

    let blob = BlobId::generate();
    let a = BlockId::parse("dup-a")?;
    let b = BlockId::parse("dup-b")?;
    let da = pattern(400, 5);
    let db = pattern(300, 11);
    store
        .stage_block(&blob, &a, BlobUpload::from_bytes(da.clone()))
        .await?;
    store
        .stage_block(&blob, &b, BlobUpload::from_bytes(db.clone()))
        .await?;

    let total = store
        .commit_blocks(&blob, &[a.clone(), b.clone(), a.clone()], u64::MAX)
        .await?;
    assert_eq!(total, 1_100);
    assert_eq!(store.size(&blob).await?, Some(1_100));

    let expected: Bytes = [da.clone(), db, da.clone()].concat().into();
    let full = ByteRange::full(1_100).ok_or("full range")?;
    assert_eq!(read_range(&store, &blob, full).await?, expected);

    let mid = ByteRange::parse_header("bytes=200-899", 1_100)?;
    assert_eq!(
        read_range(&store, &blob, mid).await?,
        expected.slice(200..=899)
    );

    let dup_only = BlobId::generate();
    store
        .stage_block(&dup_only, &a, BlobUpload::from_bytes(da.clone()))
        .await?;
    let dup_total = store
        .commit_blocks(&dup_only, &[a.clone(), a], u64::MAX)
        .await?;
    assert_eq!(dup_total, 800);
    let dup_full = ByteRange::full(800).ok_or("dup full range")?;
    assert_eq!(
        read_range(&store, &dup_only, dup_full).await?,
        [da.clone(), da].concat()
    );
    Ok(())
}

#[tokio::test]
async fn put_and_commit_cleanly_overwrite_each_other() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    let blob = BlobId::generate();
    let b1 = BlockId::parse("one")?;
    let b2 = BlockId::parse("two")?;

    store
        .stage_block(&blob, &b1, BlobUpload::from_bytes(pattern(100, 1)))
        .await?;
    store
        .stage_block(&blob, &b2, BlobUpload::from_bytes(pattern(150, 2)))
        .await?;
    store
        .commit_blocks(&blob, &[b1.clone(), b2.clone()], u64::MAX)
        .await?;
    assert_eq!(store.size(&blob).await?, Some(250));

    let replacement = pattern(75, 8);
    store
        .put(&blob, BlobUpload::from_bytes(replacement.clone()))
        .await?;
    assert_eq!(store.size(&blob).await?, Some(75));
    assert!(
        !dir.path()
            .join("blobs")
            .join(format!("{}.manifest", blob.as_str()))
            .exists()
    );
    assert!(!dir.path().join("committed").join(blob.as_str()).exists());
    let full = ByteRange::full(75).ok_or("full")?;
    assert_eq!(read_range(&store, &blob, full).await?, replacement);

    let p1 = pattern(50, 3);
    let p2 = pattern(60, 4);
    store
        .stage_block(&blob, &b1, BlobUpload::from_bytes(p1.clone()))
        .await?;
    store
        .stage_block(&blob, &b2, BlobUpload::from_bytes(p2.clone()))
        .await?;
    store.commit_blocks(&blob, &[b1, b2], u64::MAX).await?;
    assert_eq!(store.size(&blob).await?, Some(110));
    assert!(!dir.path().join("blobs").join(blob.as_str()).exists());
    let full2 = ByteRange::full(110).ok_or("full2")?;
    assert_eq!(read_range(&store, &blob, full2).await?, [p1, p2].concat());
    Ok(())
}

#[tokio::test]
async fn commit_blocks_retry_is_idempotent_and_corrupt_manifest_returns_none() -> TestResult {
    let dir = TempDir::new()?;
    let store = LocalBlobStore::open(dir.path()).await?;
    let blob = BlobId::generate();
    let b1 = BlockId::parse("retry-1")?;
    let b2 = BlockId::parse("retry-2")?;

    let d1 = pattern(120, 4);
    let d2 = pattern(180, 9);
    store
        .stage_block(&blob, &b1, BlobUpload::from_bytes(d1.clone()))
        .await?;
    store
        .stage_block(&blob, &b2, BlobUpload::from_bytes(d2.clone()))
        .await?;

    let first = store
        .commit_blocks(&blob, &[b1.clone(), b2.clone()], u64::MAX)
        .await?;
    let retry = store.commit_blocks(&blob, &[b1, b2], u64::MAX).await?;
    assert_eq!(first, 300);
    assert_eq!(retry, 300);

    let manifest_file = dir
        .path()
        .join("blobs")
        .join(format!("{}.manifest", blob.as_str()));
    std::fs::write(&manifest_file, b"corrupt\n")?;
    assert_eq!(store.size(&blob).await?, None);
    let full = ByteRange::full(300).ok_or("full")?;
    assert!(store.read(&blob, full).await.is_err());
    Ok(())
}
