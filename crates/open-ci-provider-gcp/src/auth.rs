//! Async `OAuth2` bearer token sources for GCP Compute Engine.

mod impersonate;
mod metadata;

pub use impersonate::{
    DEFAULT_COMPUTE_SCOPE, DEFAULT_IAM_CREDENTIALS_BASE_URL,
    DEFAULT_IMPERSONATION_LIFETIME_SECONDS, ImpersonatedTokenSource, ImpersonationConfig,
};
pub use metadata::{DEFAULT_GCP_METADATA_BASE_URL, DEFAULT_METADATA_ACCOUNT, MetadataTokenSource};

use async_trait::async_trait;
use open_ci_core::SecretString;
use open_ci_provider::ProviderError;
use reqwest::StatusCode;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::RwLock;

pub(crate) const REFRESH_BUFFER_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub(crate) struct CachedToken {
    pub(crate) token: SecretString,
    pub(crate) expires_at_unix: u64,
}

impl CachedToken {
    pub(crate) fn is_valid(&self, now_unix: u64, buffer_seconds: u64) -> bool {
        now_unix.saturating_add(buffer_seconds) < self.expires_at_unix
    }
}

pub(crate) fn is_safe_service_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 256
        && account != "."
        && account != ".."
        && !account.contains("..")
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
}

pub(crate) fn parse_rfc3339_unix(raw: &str) -> Result<u64, ProviderError> {
    let dt = OffsetDateTime::parse(raw.trim(), &Rfc3339).map_err(|_| {
        ProviderError::Transient("invalid RFC3339 token expiration timestamp".to_owned())
    })?;
    u64::try_from(dt.unix_timestamp())
        .map_err(|_| ProviderError::Transient("token expiration predates unix epoch".to_owned()))
}

pub(crate) fn map_transport_error(err: &reqwest::Error, message: &str) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transient(message.to_owned())
    }
}

pub(crate) fn map_http_status_error(status: StatusCode, source: &str) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            ProviderError::AuthFailed
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
            retry_after_seconds: 30,
        },
        _ => ProviderError::Transient(format!("{source} http {}", status.as_u16())),
    }
}

/// Async token source for GCP `OAuth2` bearer tokens.
#[async_trait]
pub trait GcpTokenSource: Send + Sync + std::fmt::Debug {
    async fn bearer_token(&self) -> Result<SecretString, ProviderError>;
}

/// Reads a rotated token file asynchronously and caches it in memory for `ttl`.
#[derive(Debug)]
pub struct FileTokenSource {
    path: PathBuf,
    ttl: Duration,
    cached: RwLock<Option<(SecretString, Instant)>>,
}

impl FileTokenSource {
    pub fn new(path: PathBuf, ttl: Duration) -> Self {
        Self {
            path,
            ttl,
            cached: RwLock::new(None),
        }
    }
}

#[async_trait]
impl GcpTokenSource for FileTokenSource {
    async fn bearer_token(&self) -> Result<SecretString, ProviderError> {
        {
            let guard = self.cached.read().await;
            if let Some((token, fetched_at)) = guard.as_ref()
                && fetched_at.elapsed() < self.ttl
            {
                return Ok(token.clone());
            }
        }
        let raw = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|_| ProviderError::AuthFailed)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ProviderError::AuthFailed);
        }
        let token = SecretString::new(trimmed);
        let mut guard = self.cached.write().await;
        *guard = Some((token.clone(), Instant::now()));
        Ok(token)
    }
}

/// Static token source useful for tests.
#[derive(Debug)]
pub struct StaticTokenSource(pub SecretString);

#[async_trait]
impl GcpTokenSource for StaticTokenSource {
    async fn bearer_token(&self) -> Result<SecretString, ProviderError> {
        Ok(self.0.clone())
    }
}
