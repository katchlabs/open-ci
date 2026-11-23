//! High-speed GitHub Actions v2 transparent cache service for `open-ci`.
//!
//! Implements the GitHub Actions Cache v2 Twirp HTTP protocol
//! (`github.actions.results.api.v1.CacheService`) and Azure Block Blob-compatible
//! chunked upload/download endpoints so official `actions/cache` v4+ works transparently
//! at LAN/`NVMe`/S3/R2 speed.

pub mod auth;
pub mod config;
pub mod error;
pub mod http;
pub mod index;
pub mod storage;

pub use auth::{
    BlobOp, CacheTokenClaims, issue_cache_token, sign_blob_url, verify_blob_signature,
    verify_cache_token,
};
pub use config::{
    CacheBackendConfig, CacheConfig, DEFAULT_LISTEN_ADDR, DEFAULT_MAX_ENTRY_BYTES,
    DEFAULT_MAX_TOTAL_BYTES, DEFAULT_SIGNED_URL_TTL_SECONDS, S3Config,
};
pub use error::CacheError;
pub use http::{CacheServer, CacheState, build_cache_router};
pub use index::{CacheEntry, CacheIndex};
pub use storage::{BlobPayload, CacheStorage, LocalFsStorage, S3CompatibleStorage};
