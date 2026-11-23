//! Async AWS credential sources for `open-ci-provider-aws`.

mod imds;
mod sts;

pub use imds::{DEFAULT_IMDS_BASE_URL, ImdsAwsCredentials};
pub use sts::{
    AssumeRoleConfig, DEFAULT_ASSUME_ROLE_DURATION_SECONDS, DEFAULT_STS_ENDPOINT,
    StsAssumeRoleCredentials,
};

use crate::sigv4::AwsCredentials;
use async_trait::async_trait;
use open_ci_core::{Clock, SecretString, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::Client;
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(crate) const REFRESH_BUFFER_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub(crate) struct CachedCredentials {
    pub(crate) creds: AwsCredentials,
    pub(crate) expires_at_unix: u64,
}

impl CachedCredentials {
    pub(crate) fn is_valid(&self, now_unix: u64, buffer_seconds: u64) -> bool {
        now_unix.saturating_add(buffer_seconds) < self.expires_at_unix
    }
}

pub(crate) fn parse_rfc3339_unix(raw: &str) -> Result<u64, ProviderError> {
    let dt = OffsetDateTime::parse(raw.trim(), &Rfc3339).map_err(|_| {
        ProviderError::Transient("invalid RFC3339 credential expiration timestamp".to_owned())
    })?;
    u64::try_from(dt.unix_timestamp()).map_err(|_| {
        ProviderError::Transient("credential expiration predates unix epoch".to_owned())
    })
}

/// Async source of AWS credentials (environment, static, or IMDS/STS).
#[async_trait]
pub trait AwsCredentialsSource: Send + Sync + std::fmt::Debug {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError>;
}

/// Reads standard AWS credentials from environment variables.
#[derive(Debug, Default)]
pub struct EnvAwsCredentials;

#[async_trait]
impl AwsCredentialsSource for EnvAwsCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        let access_key_id =
            std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| ProviderError::AuthFailed)?;
        let secret_access_key =
            std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| ProviderError::AuthFailed)?;
        let session_token = std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(SecretString::new);
        Ok(AwsCredentials {
            access_key_id,
            secret_access_key: SecretString::new(secret_access_key),
            session_token,
        })
    }
}

/// Static AWS credentials useful for tests and local fixtures.
#[derive(Debug)]
pub struct StaticAwsCredentials(pub AwsCredentials);

#[async_trait]
impl AwsCredentialsSource for StaticAwsCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        Ok(self.0.clone())
    }
}

#[derive(Debug)]
struct FallbackAwsCredentials {
    primary: Arc<dyn AwsCredentialsSource>,
    fallback: Arc<dyn AwsCredentialsSource>,
}

#[async_trait]
impl AwsCredentialsSource for FallbackAwsCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        match self.primary.credentials().await {
            Ok(creds) => Ok(creds),
            Err(_) => self.fallback.credentials().await,
        }
    }
}

/// Standard AWS credential resolution chain:
/// 1. Environment variables (`EnvAwsCredentials`)
/// 2. EC2 `IMDSv2` instance profile (`ImdsAwsCredentials`)
/// 3. Optional STS `AssumeRole` wrapper (`StsAssumeRoleCredentials`)
#[derive(Debug)]
pub struct AwsCredentialChain {
    inner: Arc<dyn AwsCredentialsSource>,
}

impl AwsCredentialChain {
    /// Builds the default AWS credential chain (`Env` -> `IMDSv2` -> optional STS `AssumeRole`).
    pub fn new(client: Client, assume_role: Option<AssumeRoleConfig>) -> Self {
        Self::with_imds_and_clock(
            client,
            DEFAULT_IMDS_BASE_URL,
            assume_role,
            Arc::new(SystemClock),
        )
    }

    /// Builds the chain with a custom IMDS base URL and injectable clock.
    pub fn with_imds_and_clock(
        client: Client,
        imds_base_url: impl Into<String>,
        assume_role: Option<AssumeRoleConfig>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let imds = Arc::new(ImdsAwsCredentials::with_clock(
            client.clone(),
            imds_base_url,
            Arc::clone(&clock),
        ));
        Self::with_sources(
            Arc::new(EnvAwsCredentials),
            imds,
            client,
            assume_role,
            clock,
        )
    }

    /// Builds a credential chain from explicit primary/fallback sources and optional STS `AssumeRole`.
    pub fn with_sources(
        primary: Arc<dyn AwsCredentialsSource>,
        fallback: Arc<dyn AwsCredentialsSource>,
        client: Client,
        assume_role: Option<AssumeRoleConfig>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let base: Arc<dyn AwsCredentialsSource> =
            Arc::new(FallbackAwsCredentials { primary, fallback });
        let inner: Arc<dyn AwsCredentialsSource> = match assume_role {
            Some(config) => Arc::new(StsAssumeRoleCredentials::with_clock(
                base, client, config, clock,
            )),
            None => base,
        };
        Self { inner }
    }
}

#[async_trait]
impl AwsCredentialsSource for AwsCredentialChain {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        self.inner.credentials().await
    }
}
