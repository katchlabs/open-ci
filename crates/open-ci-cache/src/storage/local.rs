//! Local filesystem / `NVMe` storage backend with atomic writes (`0o600`) and LRU eviction.

use crate::auth::{validate_blob_id, validate_block_id};
use crate::error::CacheError;
use crate::index::{CacheEntry, decode_jsonl, encode_jsonl};
use crate::storage::{BlobPayload, CacheStorage, hash_block_id};
use async_trait::async_trait;
use bytes::Bytes;
use open_ci_core::{Clock, SystemClock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use uuid::Uuid;

const COPY_BUF_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug)]
struct BlobRecord {
    size_bytes: u64,
    last_accessed_unix: u64,
    access_seq: u64,
}

#[derive(Debug, Default)]
struct LruTracker {
    blobs: HashMap<String, BlobRecord>,
    total_bytes: u64,
    next_seq: u64,
}

impl LruTracker {
    fn record_insert(&mut self, blob_id: &str, size_bytes: u64, now_unix: u64) {
        self.next_seq = self.next_seq.saturating_add(1);
        let seq = self.next_seq;
        if let Some(prev) = self.blobs.insert(
            blob_id.to_owned(),
            BlobRecord {
                size_bytes,
                last_accessed_unix: now_unix,
                access_seq: seq,
            },
        ) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.size_bytes);
        }
        self.total_bytes = self.total_bytes.saturating_add(size_bytes);
    }

    fn touch(&mut self, blob_id: &str, now_unix: u64) {
        self.next_seq = self.next_seq.saturating_add(1);
        let seq = self.next_seq;
        if let Some(rec) = self.blobs.get_mut(blob_id) {
            rec.last_accessed_unix = now_unix;
            rec.access_seq = seq;
        }
    }

    fn remove(&mut self, blob_id: &str) {
        if let Some(prev) = self.blobs.remove(blob_id) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.size_bytes);
        }
    }

    /// Selects least-recently-used blob IDs to evict until `total_bytes <= max_total_bytes`.
    fn drain_evictions(
        &mut self,
        max_total_bytes: u64,
        protected_blob_id: Option<&str>,
    ) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.total_bytes > max_total_bytes {
            let victim = self
                .blobs
                .iter()
                .filter(|(id, _)| protected_blob_id != Some(id.as_str()))
                .min_by_key(|(_, rec)| (rec.last_accessed_unix, rec.access_seq))
                .map(|(id, _)| id.clone());
            let Some(victim_id) = victim else {
                break;
            };
            self.remove(&victim_id);
            evicted.push(victim_id);
        }
        evicted
    }
}

/// Local disk / `NVMe` cache storage backend.
///
/// Stores finalized blobs under `<root>/blobs/`, staged Azure blocks under `<root>/staging/`,
/// atomic temporary files under `<root>/tmp/`, and the metadata index at `<root>/index.jsonl`.
#[derive(Debug)]
pub struct LocalFsStorage {
    root_dir: PathBuf,
    max_total_bytes: u64,
    clock: Arc<dyn Clock>,
    tracker: RwLock<LruTracker>,
}

impl LocalFsStorage {
    /// Opens (or initializes) a local filesystem storage directory and rebuilds its LRU state.
    pub async fn open(root_dir: PathBuf, max_total_bytes: u64) -> Result<Self, CacheError> {
        Self::open_with_clock(root_dir, max_total_bytes, Arc::new(SystemClock)).await
    }

    /// Opens a local filesystem storage directory with an injectable [`Clock`].
    pub async fn open_with_clock(
        root_dir: PathBuf,
        max_total_bytes: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, CacheError> {
        for sub in ["blobs", "staging", "tmp"] {
            fs::create_dir_all(root_dir.join(sub))
                .await
                .map_err(|err| CacheError::Storage(format!("failed creating {sub} dir: {err}")))?;
        }

        let storage = Self {
            root_dir,
            max_total_bytes,
            clock,
            tracker: RwLock::new(LruTracker::default()),
        };
        storage.rebuild_tracker_from_disk().await?;
        storage.evict_over_budget(None).await?;
        Ok(storage)
    }

    /// Returns the currently tracked total size of blobs on disk.
    pub async fn total_bytes(&self) -> u64 {
        self.tracker.read().await.total_bytes
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root_dir.join("blobs")
    }

    fn blob_path(&self, blob_id: &str) -> PathBuf {
        self.blobs_dir().join(blob_id)
    }

    fn staging_dir(&self, blob_id: &str) -> PathBuf {
        self.root_dir.join("staging").join(blob_id)
    }

    fn tmp_path(&self, prefix: &str) -> PathBuf {
        self.root_dir
            .join("tmp")
            .join(format!("{prefix}.{}.tmp", Uuid::new_v4()))
    }

    fn index_path(&self) -> PathBuf {
        self.root_dir.join("index.jsonl")
    }

    async fn rebuild_tracker_from_disk(&self) -> Result<(), CacheError> {
        let mut persisted_access: HashMap<String, u64> = HashMap::new();
        for entry in self.load_index().await? {
            persisted_access.insert(entry.blob_id, entry.last_accessed_at);
        }

        let mut entries = fs::read_dir(self.blobs_dir())
            .await
            .map_err(|err| CacheError::Storage(format!("failed scanning blobs dir: {err}")))?;

        let mut tracker = self.tracker.write().await;
        while let Some(dir_entry) = entries
            .next_entry()
            .await
            .map_err(|err| CacheError::Storage(format!("failed reading blobs dir entry: {err}")))?
        {
            let file_name = dir_entry.file_name();
            let Some(blob_id) = file_name.to_str() else {
                continue;
            };
            if validate_blob_id(blob_id).is_err() {
                continue;
            }
            let Ok(meta) = dir_entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let accessed = persisted_access.get(blob_id).copied().unwrap_or_else(|| {
                meta.modified()
                    .ok()
                    .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs())
            });
            tracker.record_insert(blob_id, meta.len(), accessed);
        }
        Ok(())
    }

    async fn evict_over_budget(&self, protected_blob_id: Option<&str>) -> Result<(), CacheError> {
        let victims = {
            let mut tracker = self.tracker.write().await;
            tracker.drain_evictions(self.max_total_bytes, protected_blob_id)
        };
        for victim_id in victims {
            let path = self.blob_path(&victim_id);
            if let Err(err) = fs::remove_file(&path).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(CacheError::Storage(format!(
                    "failed evicting blob {victim_id}: {err}"
                )));
            }
        }
        Ok(())
    }

    async fn write_atomic(&self, dest: &Path, data: &[u8]) -> Result<(), CacheError> {
        let tmp = self.tmp_path("blob");
        let mut file = open_private_file(&tmp).await?;
        if let Err(err) = file.write_all(data).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(CacheError::Storage(format!(
                "failed writing temp file: {err}"
            )));
        }
        if let Err(err) = file.flush().await {
            let _ = fs::remove_file(&tmp).await;
            return Err(CacheError::Storage(format!(
                "failed flushing temp file: {err}"
            )));
        }
        drop(file);
        if let Err(err) = fs::rename(&tmp, dest).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(CacheError::Storage(format!(
                "failed committing atomic file: {err}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl CacheStorage for LocalFsStorage {
    async fn put_blob(&self, blob_id: &str, data: Bytes) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        let size = data.len() as u64;
        let dest = self.blob_path(blob_id);
        self.write_atomic(&dest, &data).await?;

        let now = self.clock.now_unix();
        {
            let mut tracker = self.tracker.write().await;
            tracker.record_insert(blob_id, size, now);
        }
        self.evict_over_budget(Some(blob_id)).await?;
        Ok(size)
    }

    async fn stage_block(
        &self,
        blob_id: &str,
        block_id: &str,
        data: Bytes,
    ) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        validate_block_id(block_id)?;
        let dir = self.staging_dir(blob_id);
        fs::create_dir_all(&dir)
            .await
            .map_err(|err| CacheError::Storage(format!("failed creating staging dir: {err}")))?;

        let block_path = dir.join(hash_block_id(block_id));
        let size = data.len() as u64;
        self.write_atomic(&block_path, &data).await?;
        Ok(size)
    }

    async fn commit_blocks(
        &self,
        blob_id: &str,
        block_ids: &[String],
        max_entry_bytes: u64,
    ) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        if block_ids.is_empty() {
            return Err(CacheError::BadRequest(
                "block list must not be empty".to_owned(),
            ));
        }

        let staging_dir = self.staging_dir(blob_id);
        let tmp = self.tmp_path("commit");
        let mut out_file = open_private_file(&tmp).await?;
        let mut total_size = 0u64;
        let mut buf = vec![0u8; COPY_BUF_SIZE];

        for block_id in block_ids {
            validate_block_id(block_id)?;
            let block_path = staging_dir.join(hash_block_id(block_id));
            let mut block_file = match fs::File::open(&block_path).await {
                Ok(f) => f,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let _ = fs::remove_file(&tmp).await;
                    return Err(CacheError::BadRequest(format!(
                        "staged block not found: {block_id}"
                    )));
                }
                Err(err) => {
                    let _ = fs::remove_file(&tmp).await;
                    return Err(CacheError::Storage(format!(
                        "failed reading staged block: {err}"
                    )));
                }
            };

            loop {
                let n = block_file.read(&mut buf).await.map_err(|err| {
                    CacheError::Storage(format!("failed streaming staged block: {err}"))
                })?;
                if n == 0 {
                    break;
                }
                total_size = total_size.saturating_add(n as u64);
                if total_size > max_entry_bytes {
                    drop(out_file);
                    let _ = fs::remove_file(&tmp).await;
                    return Err(CacheError::PayloadTooLarge(format!(
                        "assembled blob exceeds max_entry_bytes ({max_entry_bytes})"
                    )));
                }
                if let Err(err) = out_file.write_all(&buf[..n]).await {
                    let _ = fs::remove_file(&tmp).await;
                    return Err(CacheError::Storage(format!(
                        "failed assembling blob: {err}"
                    )));
                }
            }
        }

        out_file
            .flush()
            .await
            .map_err(|err| CacheError::Storage(format!("failed flushing assembled blob: {err}")))?;
        drop(out_file);

        let dest = self.blob_path(blob_id);
        if let Err(err) = fs::rename(&tmp, &dest).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(CacheError::Storage(format!(
                "failed finalizing assembled blob: {err}"
            )));
        }
        let _ = fs::remove_dir_all(&staging_dir).await;

        let now = self.clock.now_unix();
        {
            let mut tracker = self.tracker.write().await;
            tracker.record_insert(blob_id, total_size, now);
        }
        self.evict_over_budget(Some(blob_id)).await?;
        Ok(total_size)
    }

    async fn get_blob(&self, blob_id: &str) -> Result<Option<BlobPayload>, CacheError> {
        validate_blob_id(blob_id)?;
        let path = self.blob_path(blob_id);
        let meta = match fs::metadata(&path).await {
            Ok(m) if m.is_file() => m,
            Ok(_) => return Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CacheError::Storage(format!(
                    "failed inspecting blob metadata: {err}"
                )));
            }
        };
        let now = self.clock.now_unix();
        self.tracker.write().await.touch(blob_id, now);
        Ok(Some(BlobPayload::File {
            path,
            size: meta.len(),
        }))
    }

    async fn head_blob(&self, blob_id: &str) -> Result<Option<u64>, CacheError> {
        validate_blob_id(blob_id)?;
        let path = self.blob_path(blob_id);
        let meta = match fs::metadata(&path).await {
            Ok(m) if m.is_file() => m,
            Ok(_) => return Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CacheError::Storage(format!(
                    "failed inspecting blob metadata: {err}"
                )));
            }
        };
        let now = self.clock.now_unix();
        self.tracker.write().await.touch(blob_id, now);
        Ok(Some(meta.len()))
    }

    async fn delete_blob(&self, blob_id: &str) -> Result<(), CacheError> {
        validate_blob_id(blob_id)?;
        let path = self.blob_path(blob_id);
        if let Err(err) = fs::remove_file(&path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(CacheError::Storage(format!(
                "failed deleting blob {blob_id}: {err}"
            )));
        }
        self.tracker.write().await.remove(blob_id);
        Ok(())
    }

    async fn load_index(&self) -> Result<Vec<CacheEntry>, CacheError> {
        let path = self.index_path();
        match fs::read_to_string(&path).await {
            Ok(raw) => Ok(decode_jsonl(&raw)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(CacheError::Storage(format!(
                "failed reading cache index: {err}"
            ))),
        }
    }

    async fn save_index(&self, entries: &[CacheEntry]) -> Result<(), CacheError> {
        let bytes = encode_jsonl(entries)
            .map_err(|err| CacheError::Storage(format!("failed serializing cache index: {err}")))?;
        self.write_atomic(&self.index_path(), &bytes).await
    }
}

async fn open_private_file(path: &Path) -> Result<fs::File, CacheError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    opts.open(path)
        .await
        .map_err(|err| CacheError::Storage(format!("failed creating temp file: {err}")))
}
