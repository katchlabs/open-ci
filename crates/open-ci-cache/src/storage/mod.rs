//! Pluggable storage abstraction and built-in backends (`LocalFsStorage`, `S3CompatibleStorage`).

pub mod local;
pub mod s3;

use crate::error::CacheError;
use crate::index::CacheEntry;
use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub use local::LocalFsStorage;
pub use s3::S3CompatibleStorage;

/// Blob payload returned by [`CacheStorage::get_blob`].
#[derive(Debug)]
pub enum BlobPayload {
    /// In-memory byte buffer (e.g. from remote object storage).
    Bytes(Bytes),
    /// On-disk file path that the HTTP layer streams without full-file buffering.
    File { path: PathBuf, size: u64 },
}

impl BlobPayload {
    /// Returns the total byte length of the blob.
    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(data) => data.len() as u64,
            Self::File { size, .. } => *size,
        }
    }

    /// Returns `true` when the blob is zero bytes long.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads the entire payload into memory (primarily useful in tests).
    pub async fn into_bytes(self) -> Result<Bytes, CacheError> {
        match self {
            Self::Bytes(data) => Ok(data),
            Self::File { path, .. } => tokio::fs::read(&path)
                .await
                .map(Bytes::from)
                .map_err(|err| CacheError::Storage(format!("failed reading blob file: {err}"))),
        }
    }
}

/// Pluggable storage backend for cache blobs, Azure-style staged blocks, and metadata index.
#[async_trait]
pub trait CacheStorage: Send + Sync + std::fmt::Debug {
    /// Writes a complete blob atomically and returns its stored size in bytes.
    async fn put_blob(&self, blob_id: &str, data: Bytes) -> Result<u64, CacheError>;

    /// Stages a single Azure-compatible chunk block for later assembly.
    async fn stage_block(
        &self,
        blob_id: &str,
        block_id: &str,
        data: Bytes,
    ) -> Result<u64, CacheError>;

    /// Assembles previously staged blocks in `block_ids` order into the final blob,
    /// enforcing `max_entry_bytes`. Returns the assembled size in bytes.
    async fn commit_blocks(
        &self,
        blob_id: &str,
        block_ids: &[String],
        max_entry_bytes: u64,
    ) -> Result<u64, CacheError>;

    /// Fetches a blob payload if present.
    async fn get_blob(&self, blob_id: &str) -> Result<Option<BlobPayload>, CacheError>;

    /// Returns the byte length of a blob if it exists.
    async fn head_blob(&self, blob_id: &str) -> Result<Option<u64>, CacheError>;

    /// Deletes a blob if present.
    async fn delete_blob(&self, blob_id: &str) -> Result<(), CacheError>;

    /// Loads persisted cache metadata entries on startup.
    async fn load_index(&self) -> Result<Vec<CacheEntry>, CacheError> {
        Ok(Vec::new())
    }

    /// Persists the current cache metadata index.
    async fn save_index(&self, _entries: &[CacheEntry]) -> Result<(), CacheError> {
        Ok(())
    }
}

/// Hashes an arbitrary Azure `blockid` into a fixed safe hex filename to prevent path traversal.
pub(crate) fn hash_block_id(block_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(block_id.as_bytes());
    hex::encode(hasher.finalize())
}
