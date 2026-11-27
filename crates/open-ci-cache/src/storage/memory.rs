//! Bounded in-memory LRU hot tier wrapping any [`BlobStore`].

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::{BlobStore, BlobUpload, ByteStream};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

const COALESCE_CHUNK_BYTES: usize = 1024 * 1024;

/// Write-through bounded RAM cache layered over an inner [`BlobStore`].
#[derive(Debug)]
pub struct HotMemoryBlobStore {
    inner: Arc<dyn BlobStore>,
    max_bytes: u64,
    tick: AtomicU64,
    in_flight_bytes: Arc<AtomicU64>,
    state: RwLock<HotState>,
}

#[derive(Debug)]
struct HotBlob {
    total_size: u64,
    chunks: Vec<Bytes>,
    offsets: Vec<u64>,
}

impl HotBlob {
    fn from_chunks(raw_chunks: Vec<Bytes>) -> Self {
        let mut chunks = Vec::with_capacity(raw_chunks.len());
        let mut offsets = Vec::with_capacity(raw_chunks.len());
        let mut total_size = 0u64;
        for chunk in raw_chunks {
            if chunk.is_empty() {
                continue;
            }
            offsets.push(total_size);
            total_size = total_size.saturating_add(chunk.len() as u64);
            chunks.push(chunk);
        }
        Self {
            total_size,
            chunks,
            offsets,
        }
    }

    fn slice(&self, range: ByteRange) -> Vec<Bytes> {
        if self.chunks.is_empty() || range.start() >= self.total_size {
            return Vec::new();
        }
        let first = self
            .offsets
            .partition_point(|&off| off <= range.start())
            .saturating_sub(1);
        let mut out = Vec::new();
        for (&chunk_start, chunk) in self.offsets.iter().zip(&self.chunks).skip(first) {
            if chunk_start > range.end_inclusive() {
                break;
            }
            let chunk_end_inclusive = chunk_start + (chunk.len() as u64) - 1;
            let slice_start = range.start().max(chunk_start) - chunk_start;
            let slice_end_exclusive =
                (range.end_inclusive().min(chunk_end_inclusive) - chunk_start) + 1;
            if let (Ok(start), Ok(end)) = (
                usize::try_from(slice_start),
                usize::try_from(slice_end_exclusive),
            ) {
                out.push(chunk.slice(start..end));
            }
        }
        out
    }
}

#[derive(Debug)]
struct HotEntry {
    blob: HotBlob,
    last_used: AtomicU64,
}

#[derive(Debug)]
struct StagedBlock {
    size: u64,
    chunks: Vec<Bytes>,
}

#[derive(Debug)]
struct StagedBlob {
    last_used: u64,
    total_bytes: u64,
    blocks: HashMap<BlockId, StagedBlock>,
}

#[derive(Debug, Default)]
struct HotState {
    committed: HashMap<BlobId, HotEntry>,
    committed_bytes: u64,
    staged: HashMap<BlobId, StagedBlob>,
    staged_bytes: u64,
}

impl HotState {
    fn total_bytes(&self) -> u64 {
        self.committed_bytes.saturating_add(self.staged_bytes)
    }

    fn remove_committed(&mut self, blob: &BlobId) {
        if let Some(removed) = self.committed.remove(blob) {
            self.committed_bytes = self.committed_bytes.saturating_sub(removed.blob.total_size);
        }
    }

    fn evict_to_fit(&mut self, needed: u64, max_bytes: u64, keep_staged: Option<&BlobId>) {
        while self.total_bytes().saturating_add(needed) > max_bytes {
            let oldest_committed = self
                .committed
                .iter()
                .min_by_key(|(_, entry)| entry.last_used.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest_committed {
                self.remove_committed(&key);
                continue;
            }
            let oldest_staged = self
                .staged
                .iter()
                .filter(|(key, _)| keep_staged != Some(*key))
                .min_by_key(|(_, staged)| staged.last_used)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest_staged {
                let _ = self.take_staged_blob(&key);
                continue;
            }
            break;
        }
    }

    fn insert_committed(&mut self, id: BlobId, blob: HotBlob, max_bytes: u64, tick: u64) {
        self.remove_committed(&id);
        if max_bytes == 0 || blob.total_size > max_bytes {
            return;
        }
        self.evict_to_fit(blob.total_size, max_bytes, None);
        if self.total_bytes().saturating_add(blob.total_size) > max_bytes {
            return;
        }
        self.committed_bytes = self.committed_bytes.saturating_add(blob.total_size);
        self.committed.insert(
            id,
            HotEntry {
                blob,
                last_used: AtomicU64::new(tick),
            },
        );
    }

    fn remove_staged_block(&mut self, blob: &BlobId, block: &BlockId) {
        let mut remove_blob = false;
        if let Some(staged) = self.staged.get_mut(blob) {
            if let Some(removed) = staged.blocks.remove(block) {
                staged.total_bytes = staged.total_bytes.saturating_sub(removed.size);
                self.staged_bytes = self.staged_bytes.saturating_sub(removed.size);
            }
            remove_blob = staged.blocks.is_empty();
        }
        if remove_blob {
            self.staged.remove(blob);
        }
    }

    fn insert_staged_block(
        &mut self,
        blob: BlobId,
        block: BlockId,
        part: StagedBlock,
        max_bytes: u64,
        tick: u64,
    ) {
        self.remove_staged_block(&blob, &block);
        if max_bytes == 0 || part.size > max_bytes {
            return;
        }
        self.evict_to_fit(part.size, max_bytes, Some(&blob));
        if self.total_bytes().saturating_add(part.size) > max_bytes {
            return;
        }
        self.staged_bytes = self.staged_bytes.saturating_add(part.size);
        let entry = self.staged.entry(blob).or_insert_with(|| StagedBlob {
            last_used: tick,
            total_bytes: 0,
            blocks: HashMap::new(),
        });
        entry.last_used = tick;
        entry.total_bytes = entry.total_bytes.saturating_add(part.size);
        entry.blocks.insert(block, part);
    }

    fn take_staged_blob(&mut self, blob: &BlobId) -> Option<HashMap<BlockId, StagedBlock>> {
        let staged = self.staged.remove(blob)?;
        self.staged_bytes = self.staged_bytes.saturating_sub(staged.total_bytes);
        Some(staged.blocks)
    }

    fn remove_blob(&mut self, blob: &BlobId) {
        self.remove_committed(blob);
        let _ = self.take_staged_blob(blob);
    }
}

struct InFlightPermit {
    counter: Arc<AtomicU64>,
    bytes: u64,
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

struct CapturedUpload {
    buf: Arc<Mutex<Vec<Bytes>>>,
    _permit: InFlightPermit,
}

impl HotMemoryBlobStore {
    pub fn new(inner: Arc<dyn BlobStore>, max_bytes: u64) -> Self {
        Self {
            inner,
            max_bytes,
            tick: AtomicU64::new(0),
            in_flight_bytes: Arc::new(AtomicU64::new(0)),
            state: RwLock::new(HotState::default()),
        }
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed).saturating_add(1)
    }

    fn read_state(&self) -> Result<RwLockReadGuard<'_, HotState>, CacheError> {
        self.state
            .read()
            .map_err(|_| CacheError::Internal("hot cache read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> Result<RwLockWriteGuard<'_, HotState>, CacheError> {
        self.state
            .write()
            .map_err(|_| CacheError::Internal("hot cache write lock poisoned".to_owned()))
    }

    fn try_reserve_in_flight(&self, bytes: u64) -> Option<InFlightPermit> {
        if self.max_bytes == 0 || bytes > self.max_bytes {
            return None;
        }
        let mut current = self.in_flight_bytes.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(bytes)?;
            if next > self.max_bytes {
                return None;
            }
            match self.in_flight_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(InFlightPermit {
                        counter: Arc::clone(&self.in_flight_bytes),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn tee_upload(&self, upload: BlobUpload) -> (BlobUpload, Option<CapturedUpload>) {
        let Some(permit) = self.try_reserve_in_flight(upload.content_length()) else {
            return (upload, None);
        };
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let teed = upload.map_stream(move |stream| {
            stream
                .inspect(move |item| {
                    if let Ok(chunk) = item
                        && !chunk.is_empty()
                        && let Ok(mut guard) = sink.lock()
                    {
                        guard.push(chunk.clone());
                    }
                })
                .boxed()
        });
        (
            teed,
            Some(CapturedUpload {
                buf,
                _permit: permit,
            }),
        )
    }
}

fn take_captured(buf: &Mutex<Vec<Bytes>>) -> Result<Vec<Bytes>, CacheError> {
    let raw = buf
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .map_err(|_| CacheError::Internal("hot cache capture lock poisoned".to_owned()))?;
    Ok(coalesce_chunks(raw))
}

fn coalesce_chunks(raw: Vec<Bytes>) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut current = BytesMut::new();
    for chunk in raw {
        if chunk.is_empty() {
            continue;
        }
        if current.is_empty() && chunk.len() >= COALESCE_CHUNK_BYTES {
            out.push(chunk);
            continue;
        }
        if current.len() + chunk.len() > COALESCE_CHUNK_BYTES && !current.is_empty() {
            out.push(current.split().freeze());
        }
        if current.is_empty() && chunk.len() >= COALESCE_CHUNK_BYTES {
            out.push(chunk);
            continue;
        }
        if current.capacity() < COALESCE_CHUNK_BYTES {
            current.reserve(COALESCE_CHUNK_BYTES.saturating_sub(current.len()));
        }
        current.extend_from_slice(&chunk);
        if current.len() >= COALESCE_CHUNK_BYTES {
            out.push(current.split().freeze());
        }
    }
    if !current.is_empty() {
        out.push(current.freeze());
    }
    out
}

#[async_trait]
impl BlobStore for HotMemoryBlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        let content_length = upload.content_length();
        let (upload, captured) = self.tee_upload(upload);
        self.inner.put(blob, upload).await?;

        let hot = match captured {
            Some(cap) => {
                let candidate = HotBlob::from_chunks(take_captured(&cap.buf)?);
                (candidate.total_size == content_length).then_some(candidate)
            }
            None => None,
        };

        let tick = self.next_tick();
        let mut state = self.write_state()?;
        if let Some(hot) = hot {
            state.insert_committed(blob.clone(), hot, self.max_bytes, tick);
        } else {
            state.remove_committed(blob);
        }
        Ok(())
    }

    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError> {
        let content_length = upload.content_length();
        let (upload, captured) = self.tee_upload(upload);
        self.inner.stage_block(blob, block, upload).await?;

        let part = match captured {
            Some(cap) => {
                let chunks = take_captured(&cap.buf)?;
                let size: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
                (size == content_length).then_some(StagedBlock { size, chunks })
            }
            None => None,
        };

        let tick = self.next_tick();
        let mut state = self.write_state()?;
        if let Some(part) = part {
            state.insert_staged_block(blob.clone(), block.clone(), part, self.max_bytes, tick);
        } else {
            state.remove_staged_block(blob, block);
        }
        Ok(())
    }

    async fn commit_blocks(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
        max_size: u64,
    ) -> Result<u64, CacheError> {
        let committed_size = self.inner.commit_blocks(blob, blocks, max_size).await?;

        let staged = {
            let mut state = self.write_state()?;
            let taken = state.take_staged_blob(blob);
            if taken.is_some() {
                state.remove_committed(blob);
            }
            taken
        };

        if self.max_bytes > 0
            && committed_size <= self.max_bytes
            && let Some(staged_blocks) = staged
            && blocks.iter().all(|id| staged_blocks.contains_key(id))
        {
            let mut joined = Vec::new();
            for id in blocks {
                if let Some(part) = staged_blocks.get(id) {
                    joined.extend(part.chunks.iter().cloned());
                }
            }
            let hot = HotBlob::from_chunks(joined);
            if hot.total_size == committed_size {
                let tick = self.next_tick();
                let mut state = self.write_state()?;
                state.insert_committed(blob.clone(), hot, self.max_bytes, tick);
            }
        }
        Ok(committed_size)
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        let hit = {
            let state = self.read_state()?;
            state.committed.get(blob).map(|entry| {
                entry.last_used.store(self.next_tick(), Ordering::Relaxed);
                entry.blob.total_size
            })
        };
        if let Some(size) = hit {
            return Ok(Some(size));
        }
        self.inner.size(blob).await
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        let hit = {
            let state = self.read_state()?;
            state.committed.get(blob).map(|entry| {
                entry.last_used.store(self.next_tick(), Ordering::Relaxed);
                entry.blob.slice(range)
            })
        };
        if let Some(slices) = hit {
            return Ok(futures_util::stream::iter(slices.into_iter().map(Ok)).boxed());
        }
        self.inner.read(blob, range).await
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        {
            let mut state = self.write_state()?;
            state.remove_blob(blob);
        }
        self.inner.delete(blob).await
    }
}
