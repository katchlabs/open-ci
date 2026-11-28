//! Concurrent save stampede proofs: parallel multi-block uploads through `LocalBlobStore` and
//! `HotMemoryBlobStore`, plus a probe that evicted RAM chunks are freed outside the state lock.

use bytes::Bytes;
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt, stream};
use open_ci_cache::{
    BlobId, BlobStore, BlobUpload, BlockId, ByteRange, HotMemoryBlobStore, LocalBlobStore,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Barrier;

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;

const MIB: usize = 1024 * 1024;
const UPLOADS: usize = 8;
const BLOCKS: usize = 4;
const WIRE_CHUNK: usize = 64 * 1024;
const STAMPEDE_TIMEOUT: Duration = Duration::from_secs(60);

fn pattern(len: usize, seed: u64) -> Bytes {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect::<Vec<u8>>()
        .into()
}

fn block_len(upload: usize, block: usize) -> usize {
    MIB + 4_099 * (upload + 1) + 517 * block
}

fn block_data(upload: usize, block: usize) -> Bytes {
    pattern(
        block_len(upload, block),
        ((upload as u64) << 8) | block as u64,
    )
}

fn expected_blob(upload: usize) -> Bytes {
    (0..BLOCKS)
        .map(|block| block_data(upload, block))
        .collect::<Vec<_>>()
        .concat()
        .into()
}

/// Streams `data` in small wire-sized frames; when `gate` is set, every upload must reach it
/// before any can finish, so a store that serializes uploads deadlocks and times out.
fn gated_upload(data: Bytes, gate: Option<Arc<Barrier>>) -> BlobUpload {
    let len = data.len() as u64;
    let body = stream::unfold((data, gate, 0usize), |(data, gate, sent)| async move {
        if sent >= data.len() {
            return None;
        }
        if sent == WIRE_CHUNK
            && let Some(barrier) = &gate
        {
            barrier.wait().await;
        }
        let end = (sent + WIRE_CHUNK).min(data.len());
        Some((Ok(data.slice(sent..end)), (data, gate, end)))
    })
    .boxed();
    BlobUpload::new(len, body)
}

async fn read_range(
    store: &dyn BlobStore,
    blob: &BlobId,
    range: ByteRange,
) -> Result<Bytes, TestError> {
    let chunks: Vec<Bytes> = store.read(blob, range).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

async fn verify_blob(store: &dyn BlobStore, blob: &BlobId, upload: usize) -> TestResult {
    let expected = expected_blob(upload);
    let total = expected.len() as u64;
    if store.size(blob).await? != Some(total) {
        return Err(format!("upload {upload}: wrong size").into());
    }
    let full = ByteRange::full(total).ok_or("empty blob")?;
    if read_range(store, blob, full).await? != expected {
        return Err(format!("upload {upload}: full read corrupted").into());
    }
    let boundary = block_len(upload, 0);
    let start = boundary - 1_000;
    let end = boundary + block_len(upload, 1) + 1_000;
    let spanning = ByteRange::parse_header(&format!("bytes={start}-{end}"), total)?;
    if read_range(store, blob, spanning).await? != expected.slice(start..=end) {
        return Err(format!("upload {upload}: cross-block range corrupted").into());
    }
    Ok(())
}

async fn save_and_restore(
    store: Arc<dyn BlobStore>,
    upload: usize,
    gate: Arc<Barrier>,
) -> Result<(usize, BlobId), TestError> {
    let blob = BlobId::generate();
    let ids = (0..BLOCKS)
        .map(|block| BlockId::parse(&format!("u{upload}-b{block}")))
        .collect::<Result<Vec<_>, _>>()?;
    try_join_all(ids.iter().enumerate().rev().map(|(block, id)| {
        let gate = (block == BLOCKS - 1).then(|| Arc::clone(&gate));
        store.stage_block(&blob, id, gated_upload(block_data(upload, block), gate))
    }))
    .await?;
    let total = store.commit_blocks(&blob, &ids, u64::MAX).await?;
    if total != expected_blob(upload).len() as u64 {
        return Err(format!("upload {upload}: commit returned {total}").into());
    }
    verify_blob(store.as_ref(), &blob, upload).await?;
    Ok((upload, blob))
}

async fn run_stampede(store: Arc<dyn BlobStore>) -> Result<Vec<(usize, BlobId)>, TestError> {
    let gate = Arc::new(Barrier::new(UPLOADS));
    let tasks = (0..UPLOADS)
        .map(|upload| {
            tokio::spawn(save_and_restore(
                Arc::clone(&store),
                upload,
                Arc::clone(&gate),
            ))
        })
        .collect::<Vec<_>>();
    let joined = tokio::time::timeout(STAMPEDE_TIMEOUT, futures_util::future::join_all(tasks))
        .await
        .map_err(|_| "concurrent uploads did not all make progress together (serialized store)")?;
    joined
        .into_iter()
        .map(|task| task?)
        .collect::<Result<Vec<_>, _>>()
}

fn assert_dir_empty(root: &Path, dir: &str) -> TestResult {
    let leftovers = std::fs::read_dir(root.join(dir))?.count();
    if leftovers != 0 {
        return Err(format!("{dir}/ still holds {leftovers} entries after the stampede").into());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_store_completes_concurrent_multi_block_saves_in_parallel() -> TestResult {
    let dir = TempDir::new()?;
    let local = Arc::new(LocalBlobStore::open(dir.path()).await?);
    let saved = run_stampede(Arc::clone(&local) as Arc<dyn BlobStore>).await?;
    if saved.len() != UPLOADS {
        return Err("missing uploads".into());
    }
    for (upload, blob) in &saved {
        verify_blob(local.as_ref(), blob, *upload).await?;
    }
    assert_dir_empty(dir.path(), "tmp")?;
    assert_dir_empty(dir.path(), "staging")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_memory_store_stays_bounded_and_exact_under_concurrent_saves() -> TestResult {
    let budget = 6 * MIB as u64;
    let dir = TempDir::new()?;
    let local = Arc::new(LocalBlobStore::open(dir.path()).await?);
    let hot = Arc::new(HotMemoryBlobStore::new(
        Arc::clone(&local) as Arc<dyn BlobStore>,
        budget,
    ));

    let done = Arc::new(AtomicBool::new(false));
    let peak_resident = Arc::new(AtomicU64::new(0));
    let peak_in_flight = Arc::new(AtomicU64::new(0));
    let sampler = tokio::spawn({
        let hot = Arc::clone(&hot);
        let done = Arc::clone(&done);
        let peak_resident = Arc::clone(&peak_resident);
        let peak_in_flight = Arc::clone(&peak_in_flight);
        async move {
            while !done.load(Ordering::Relaxed) {
                if let Ok(usage) = hot.usage() {
                    peak_resident.fetch_max(
                        usage.committed_bytes + usage.staged_bytes,
                        Ordering::Relaxed,
                    );
                    peak_in_flight.fetch_max(usage.in_flight_bytes, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });

    let saved = run_stampede(Arc::clone(&hot) as Arc<dyn BlobStore>).await;
    done.store(true, Ordering::Relaxed);
    sampler.await?;
    let saved = saved?;

    let total_uploaded: u64 = (0..UPLOADS).map(|u| expected_blob(u).len() as u64).sum();
    if total_uploaded <= budget * 4 {
        return Err("stampede must overflow the RAM budget to exercise eviction".into());
    }
    let peak_resident = peak_resident.load(Ordering::Relaxed);
    let peak_in_flight = peak_in_flight.load(Ordering::Relaxed);
    if peak_resident > budget || peak_in_flight > budget {
        return Err(format!(
            "RAM exceeded budget {budget}: resident {peak_resident}, in-flight {peak_in_flight}"
        )
        .into());
    }
    if peak_resident == 0 {
        return Err("hot tier never cached anything during the stampede".into());
    }

    let usage = hot.usage()?;
    if usage.in_flight_bytes != 0 || usage.staged_bytes != 0 || usage.committed_bytes > budget {
        return Err(format!("hot tier leaked after the stampede: {usage:?}").into());
    }
    for (upload, blob) in &saved {
        verify_blob(hot.as_ref(), blob, *upload).await?;
        verify_blob(local.as_ref(), blob, *upload).await?;
    }
    assert_dir_empty(dir.path(), "tmp")?;
    assert_dir_empty(dir.path(), "staging")
}

#[derive(Debug, Default)]
struct LockProbe {
    store: OnceLock<Weak<HotMemoryBlobStore>>,
    drops: Mutex<Vec<bool>>,
}

impl LockProbe {
    fn drops(&self) -> Vec<bool> {
        self.drops.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Chunk owner that, when its memory is released, checks from another thread whether the hot
/// tier's state lock is still free; a free performed under the write guard blocks that check.
#[derive(Debug)]
struct ProbedChunk {
    data: Vec<u8>,
    probe: Arc<LockProbe>,
}

impl AsRef<[u8]> for ProbedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for ProbedChunk {
    fn drop(&mut self) {
        let Some(store) = self.probe.store.get().and_then(Weak::upgrade) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(store.usage().is_ok());
        });
        let lock_free = rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
        if let Ok(mut drops) = self.probe.drops.lock() {
            drops.push(lock_free);
        }
    }
}

fn probed(len: usize, seed: u64, probe: &Arc<LockProbe>) -> Bytes {
    Bytes::from_owner(ProbedChunk {
        data: pattern(len, seed).to_vec(),
        probe: Arc::clone(probe),
    })
}

#[tokio::test]
async fn hot_memory_store_frees_evicted_chunks_outside_state_lock() -> TestResult {
    let dir = TempDir::new()?;
    let local = Arc::new(LocalBlobStore::open(dir.path()).await?);
    let hot = Arc::new(HotMemoryBlobStore::new(local, (MIB + MIB / 2) as u64));
    let probe = Arc::new(LockProbe::default());
    probe
        .store
        .set(Arc::downgrade(&hot))
        .map_err(|_| "probe already bound")?;

    let committed = BlobId::generate();
    hot.put(&committed, BlobUpload::from_bytes(probed(MIB, 1, &probe)))
        .await?;
    if hot.usage()?.committed_bytes != MIB as u64 || !probe.drops().is_empty() {
        return Err("committed chunk must stay resident in RAM before eviction".into());
    }
    hot.put(&BlobId::generate(), BlobUpload::from_bytes(pattern(MIB, 2)))
        .await?;

    let staged = BlobId::generate();
    hot.stage_block(
        &staged,
        &BlockId::parse("probe")?,
        BlobUpload::from_bytes(probed(MIB, 3, &probe)),
    )
    .await?;
    if hot.usage()?.staged_bytes != MIB as u64 {
        return Err("staged chunk must stay resident in RAM before eviction".into());
    }
    hot.stage_block(
        &BlobId::generate(),
        &BlockId::parse("other")?,
        BlobUpload::from_bytes(pattern(MIB, 4)),
    )
    .await?;

    let deleted = BlobId::generate();
    hot.put(&deleted, BlobUpload::from_bytes(probed(MIB, 5, &probe)))
        .await?;
    hot.delete(&deleted).await?;

    let drops = probe.drops();
    if drops != [true, true, true] {
        return Err(format!(
            "evicted chunks must be freed after the write guard drops (lock free per drop: {drops:?})"
        )
        .into());
    }
    Ok(())
}
