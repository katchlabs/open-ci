//! `HotMemoryBlobStore` against the shared blob contract, plus RAM hit and LRU eviction proofs.

mod support {
    pub mod blob_contract;
}

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use open_ci_cache::{
    BlobId, BlobStore, BlobUpload, BlockId, ByteRange, ByteStream, CacheError, HotMemoryBlobStore,
    LocalBlobStore,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::blob_contract::{TestResult, assert_blob_store_contract};
use tempfile::TempDir;

#[derive(Debug)]
struct CountingBlobStore {
    inner: LocalBlobStore,
    size_calls: AtomicUsize,
    read_calls: AtomicUsize,
}

impl CountingBlobStore {
    fn new(inner: LocalBlobStore) -> Self {
        Self {
            inner,
            size_calls: AtomicUsize::new(0),
            read_calls: AtomicUsize::new(0),
        }
    }

    fn size_calls(&self) -> usize {
        self.size_calls.load(Ordering::SeqCst)
    }

    fn read_calls(&self) -> usize {
        self.read_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BlobStore for CountingBlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        self.inner.put(blob, upload).await
    }

    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError> {
        self.inner.stage_block(blob, block, upload).await
    }

    async fn commit_blocks(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
        max_size: u64,
    ) -> Result<u64, CacheError> {
        self.inner.commit_blocks(blob, blocks, max_size).await
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        self.size_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.size(blob).await
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read(blob, range).await
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        self.inner.delete(blob).await
    }
}

fn pattern(len: usize, seed: u8) -> Bytes {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|b| seed.wrapping_add(b))
        .collect::<Vec<u8>>()
        .into()
}

async fn read_range(
    store: &dyn BlobStore,
    blob: &BlobId,
    range: ByteRange,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

#[tokio::test]
async fn hot_memory_blob_store_satisfies_contract() -> TestResult {
    let dir = TempDir::new()?;
    let inner = Arc::new(LocalBlobStore::open(dir.path()).await?);
    let store = HotMemoryBlobStore::new(inner, 32 * 1024 * 1024);
    assert_blob_store_contract(&store).await
}

#[tokio::test]
async fn hot_memory_blob_store_serves_hits_from_ram_without_inner_reads() -> TestResult {
    let dir = TempDir::new()?;
    let counting = Arc::new(CountingBlobStore::new(
        LocalBlobStore::open(dir.path()).await?,
    ));
    let store = HotMemoryBlobStore::new(Arc::clone(&counting) as Arc<dyn BlobStore>, 1024 * 1024);

    let single = BlobId::generate();
    let payload = pattern(5_000, 3);
    store
        .put(&single, BlobUpload::from_bytes(payload.clone()))
        .await?;
    assert_eq!(store.size(&single).await?, Some(5_000));
    let full = ByteRange::full(5_000).ok_or("full")?;
    assert_eq!(read_range(&store, &single, full).await?, payload);
    let slice = ByteRange::parse_header("bytes=123-4567", 5_000)?;
    assert_eq!(
        read_range(&store, &single, slice).await?,
        payload.slice(123..=4567)
    );
    assert_eq!(counting.size_calls(), 0);
    assert_eq!(counting.read_calls(), 0);

    let multi = BlobId::generate();
    let b1 = BlockId::parse("part-1")?;
    let b2 = BlockId::parse("part-2")?;
    let b3 = BlockId::parse("part-3")?;
    let d1 = pattern(1_000, 1);
    let d2 = pattern(2_000, 2);
    let d3 = pattern(1_500, 4);
    store
        .stage_block(&multi, &b1, BlobUpload::from_bytes(d1.clone()))
        .await?;
    store
        .stage_block(&multi, &b2, BlobUpload::from_bytes(d2.clone()))
        .await?;
    store
        .stage_block(&multi, &b3, BlobUpload::from_bytes(d3.clone()))
        .await?;
    let total = store.commit_blocks(&multi, &[b1, b2, b3], u64::MAX).await?;
    assert_eq!(total, 4_500);

    let expected: Bytes = [d1, d2, d3].concat().into();
    assert_eq!(store.size(&multi).await?, Some(4_500));
    let cross = ByteRange::parse_header("bytes=500-3499", 4_500)?;
    assert_eq!(
        read_range(&store, &multi, cross).await?,
        expected.slice(500..=3499)
    );
    assert_eq!(counting.size_calls(), 0);
    assert_eq!(counting.read_calls(), 0);
    Ok(())
}

#[tokio::test]
async fn hot_memory_blob_store_evicts_lru_entries_above_budget() -> TestResult {
    let dir = TempDir::new()?;
    let counting = Arc::new(CountingBlobStore::new(
        LocalBlobStore::open(dir.path()).await?,
    ));
    let store = HotMemoryBlobStore::new(Arc::clone(&counting) as Arc<dyn BlobStore>, 100);

    let blob_a = BlobId::generate();
    let blob_b = BlobId::generate();
    let blob_c = BlobId::generate();
    let blob_d = BlobId::generate();
    let blob_e = BlobId::generate();
    let da = pattern(60, 1);
    let db = pattern(60, 2);
    let dc = pattern(40, 3);
    let dd = pattern(40, 4);
    let de = pattern(150, 5);

    store
        .put(&blob_a, BlobUpload::from_bytes(da.clone()))
        .await?;
    store
        .put(&blob_b, BlobUpload::from_bytes(db.clone()))
        .await?;

    let full60 = ByteRange::full(60).ok_or("full60")?;
    let full40 = ByteRange::full(40).ok_or("full40")?;
    let full150 = ByteRange::full(150).ok_or("full150")?;

    assert_eq!(read_range(&store, &blob_b, full60).await?, db);
    assert_eq!(counting.read_calls(), 0);

    assert_eq!(read_range(&store, &blob_a, full60).await?, da);
    assert_eq!(counting.read_calls(), 1);

    store
        .put(&blob_c, BlobUpload::from_bytes(dc.clone()))
        .await?;
    assert_eq!(store.size(&blob_b).await?, Some(60));
    assert_eq!(counting.size_calls(), 0);

    store
        .put(&blob_d, BlobUpload::from_bytes(dd.clone()))
        .await?;
    assert_eq!(read_range(&store, &blob_b, full60).await?, db);
    assert_eq!(read_range(&store, &blob_d, full40).await?, dd);
    assert_eq!(counting.read_calls(), 1);

    assert_eq!(read_range(&store, &blob_c, full40).await?, dc);
    assert_eq!(counting.read_calls(), 2);

    store
        .put(&blob_e, BlobUpload::from_bytes(de.clone()))
        .await?;
    assert_eq!(read_range(&store, &blob_e, full150).await?, de);
    assert_eq!(counting.read_calls(), 3);

    store.delete(&blob_b).await?;
    assert_eq!(store.size(&blob_b).await?, None);
    assert!(matches!(
        store.read(&blob_b, full60).await,
        Err(CacheError::NotFound(_))
    ));
    Ok(())
}

#[tokio::test]
async fn hot_memory_blob_store_bounds_staged_blocks_and_evicts_abandoned_uploads() -> TestResult {
    let dir = TempDir::new()?;
    let counting = Arc::new(CountingBlobStore::new(
        LocalBlobStore::open(dir.path()).await?,
    ));
    let store = HotMemoryBlobStore::new(Arc::clone(&counting) as Arc<dyn BlobStore>, 100);

    let abandoned_a = BlobId::generate();
    let abandoned_b = BlobId::generate();
    let b1 = BlockId::parse("s1")?;
    store
        .stage_block(&abandoned_a, &b1, BlobUpload::from_bytes(pattern(70, 1)))
        .await?;
    store
        .stage_block(&abandoned_b, &b1, BlobUpload::from_bytes(pattern(70, 2)))
        .await?;

    let fresh = BlobId::generate();
    let fresh_data = pattern(80, 7);
    store
        .put(&fresh, BlobUpload::from_bytes(fresh_data.clone()))
        .await?;
    let full80 = ByteRange::full(80).ok_or("full80")?;
    assert_eq!(read_range(&store, &fresh, full80).await?, fresh_data);
    assert_eq!(counting.read_calls(), 0);
    Ok(())
}
