//! Runtime settings for [`crate::CacheServer`], resolved from `open-ci.toml` by the CLI.

use crate::error::CacheError;
use crate::index::RetentionPolicy;
use open_ci_core::SecretString;
use std::time::Duration;

const MIN_SIGNING_KEY_BYTES: usize = 32;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub listen_addr: String,
    /// Base URL runners reach this service on; also the origin of signed blob URLs.
    pub public_base_url: String,
    /// HMAC key for signed blob URLs.
    pub signing_key: SecretString,
    pub max_entry_bytes: u64,
    pub signed_url_ttl_seconds: u64,
    /// GitHub results service receiving every non-cache Twirp request.
    pub upstream_results_url: String,
    pub retention: RetentionPolicy,
    pub sweep_interval: Duration,
}

impl CacheConfig {
    pub fn validate(&self) -> Result<(), CacheError> {
        let invalid = |msg: &str| Err(CacheError::InvalidConfig(msg.to_owned()));
        if self.signing_key.expose().len() < MIN_SIGNING_KEY_BYTES {
            return invalid("signing key must be at least 32 bytes");
        }
        for url in [&self.public_base_url, &self.upstream_results_url] {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return invalid("cache URLs must start with http:// or https://");
            }
        }
        if self.max_entry_bytes == 0
            || self.signed_url_ttl_seconds == 0
            || self.sweep_interval.is_zero()
            || self.retention.max_total_bytes < self.max_entry_bytes
        {
            return invalid(
                "entry size, URL TTL and sweep interval must be positive and the total budget must hold one entry",
            );
        }
        Ok(())
    }
}
