//! GitHub Actions Cache Service v2 server for `open-ci` runners.
//!
//! Official `actions/cache` (and every toolkit-based cache client) talks to this service when the
//! runner's `ACTIONS_RESULTS_URL` points here. Requests are authorized by the job's own runtime
//! token, entries are isolated per repository and Git ref exactly like GitHub's cache, metadata
//! lives in a `SQLite` [`CacheIndex`], and archives stream to a pluggable [`BlobStore`].

pub mod capability;
pub mod config;
pub mod error;
pub mod http;
pub mod ids;
pub mod index;
mod retention;
pub mod storage;

pub use config::CacheConfig;
pub use error::CacheError;
pub use http::CacheServer;
pub use ids::{BlobId, BlockId, ByteRange};
pub use index::{
    CacheEntry, CacheIndex, EntryKey, Reservation, RestoreQuery, RetentionPolicy, Swept,
};
pub use storage::{BlobStore, BlobUpload, ByteStream, LocalBlobStore};
pub use storage::{GcsBlobStore, GcsStoreConfig, S3BlobStore, S3StoreConfig};
