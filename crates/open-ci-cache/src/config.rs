//! Configuration types for the `open-ci-cache` service and storage backends.

use crate::error::CacheError;
use open_ci_core::SecretString;
use std::path::PathBuf;

/// Default maximum size of a single cache entry (10 GiB).
pub const DEFAULT_MAX_ENTRY_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Default maximum total size for local filesystem storage (50 GiB).
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// Default TTL for signed upload/download URLs (15 minutes).
pub const DEFAULT_SIGNED_URL_TTL_SECONDS: u64 = 900;
/// Default listen address for the cache server.
pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8090";

/// Top-level configuration for [`crate::CacheServer`].
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Socket address to bind, e.g. `0.0.0.0:8090`.
    pub listen_addr: String,
    /// Publicly reachable base URL used when minting signed blob URLs,
    /// e.g. `http://10.0.0.5:8090`.
    pub public_base_url: String,
    /// Shared HMAC-SHA256 signing key for bearer tokens and signed blob URLs.
    pub signing_key: SecretString,
    /// Maximum allowed size of a single cache blob in bytes.
    pub max_entry_bytes: u64,
    /// Maximum total bytes retained by local storage before LRU eviction.
    pub max_total_bytes: u64,
    /// Lifetime in seconds for minted signed upload and download URLs.
    pub signed_url_ttl_seconds: u64,
    /// Storage backend selection.
    pub backend: CacheBackendConfig,
}

/// Storage backend configuration.
#[derive(Clone, Debug)]
pub enum CacheBackendConfig {
    /// Local filesystem / `NVMe` directory backend with LRU eviction.
    LocalFs { root_dir: PathBuf },
    /// Pure-Rust `SigV4` S3-compatible object storage (AWS S3, R2, `MinIO`, GCS).
    S3(S3Config),
}

/// Configuration for [`crate::S3CompatibleStorage`].
#[derive(Clone, Debug)]
pub struct S3Config {
    /// Endpoint URL, e.g. `https://s3.us-east-1.amazonaws.com` or `http://127.0.0.1:9000`.
    pub endpoint: String,
    /// AWS region or `"auto"` for Cloudflare R2.
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    /// Optional key prefix (e.g. `"open-ci-cache/"`).
    pub prefix: String,
    /// Access key ID.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: SecretString,
    /// Optional temporary session token.
    pub session_token: Option<SecretString>,
}

impl CacheConfig {
    /// Creates a [`CacheConfig`] backed by a local filesystem directory with safe defaults.
    pub fn local_fs(
        public_base_url: impl Into<String>,
        signing_key: SecretString,
        root_dir: PathBuf,
    ) -> Self {
        Self {
            listen_addr: DEFAULT_LISTEN_ADDR.to_owned(),
            public_base_url: public_base_url.into(),
            signing_key,
            max_entry_bytes: DEFAULT_MAX_ENTRY_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            signed_url_ttl_seconds: DEFAULT_SIGNED_URL_TTL_SECONDS,
            backend: CacheBackendConfig::LocalFs { root_dir },
        }
    }

    /// Validates configuration invariants before binding or handling requests.
    pub fn validate(&self) -> Result<(), CacheError> {
        if self.signing_key.is_empty() {
            return Err(CacheError::InvalidConfig(
                "signing_key must not be empty".to_owned(),
            ));
        }
        let base = self.public_base_url.trim();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(CacheError::InvalidConfig(
                "public_base_url must start with http:// or https://".to_owned(),
            ));
        }
        if self.max_entry_bytes == 0 {
            return Err(CacheError::InvalidConfig(
                "max_entry_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.max_total_bytes == 0 {
            return Err(CacheError::InvalidConfig(
                "max_total_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.signed_url_ttl_seconds == 0 {
            return Err(CacheError::InvalidConfig(
                "signed_url_ttl_seconds must be greater than zero".to_owned(),
            ));
        }
        if let CacheBackendConfig::S3(s3) = &self.backend {
            s3.validate()?;
        }
        Ok(())
    }
}

impl S3Config {
    /// Validates S3 configuration fields.
    pub fn validate(&self) -> Result<(), CacheError> {
        let endpoint = self.endpoint.trim();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(CacheError::InvalidConfig(
                "s3 endpoint must start with http:// or https://".to_owned(),
            ));
        }
        if self.region.trim().is_empty() {
            return Err(CacheError::InvalidConfig(
                "s3 region must not be empty".to_owned(),
            ));
        }
        if self.bucket.trim().is_empty() || self.bucket.contains('/') {
            return Err(CacheError::InvalidConfig(
                "s3 bucket must be non-empty and contain no slashes".to_owned(),
            ));
        }
        if self.access_key_id.trim().is_empty() || self.secret_access_key.is_empty() {
            return Err(CacheError::InvalidConfig(
                "s3 credentials must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}
