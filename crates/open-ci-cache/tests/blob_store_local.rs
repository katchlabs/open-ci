//! `LocalBlobStore` against the shared blob store contract, plus on-disk hygiene.

mod support {
    pub mod blob_contract;
}

use futures_util::{StreamExt, stream};
use open_ci_cache::{BlobId, BlobStore, BlobUpload, LocalBlobStore};
use support::blob_contract::{TestResult, assert_blob_store_contract};
use tempfile::TempDir;

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
        .put(
            &blob,
            BlobUpload::from_bytes(bytes::Bytes::from_static(b"cache")),
        )
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
    let body = stream::once(async { Ok(bytes::Bytes::from_static(b"partial")) })
        .chain(stream::once(async move {
            let _ = body_stalled.send(());
            std::future::pending::<std::io::Result<bytes::Bytes>>().await
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
