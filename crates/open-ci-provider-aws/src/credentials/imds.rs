//! EC2 `IMDSv2` temporary IAM role credential provider.

use crate::credentials::{CachedCredentials, REFRESH_BUFFER_SECONDS, parse_rfc3339_unix};
use crate::sigv4::AwsCredentials;
use async_trait::async_trait;
use open_ci_core::{Clock, SecretString, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Standard EC2 Instance Metadata Service (`IMDSv2`) link-local endpoint.
pub const DEFAULT_IMDS_BASE_URL: &str = "http://169.254.169.254";
const IMDS_TOKEN_TTL_HEADER: &str = "X-aws-ec2-metadata-token-ttl-seconds";
const IMDS_TOKEN_HEADER: &str = "X-aws-ec2-metadata-token";
const DEFAULT_TOKEN_TTL_SECONDS: &str = "21600";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImdsRoleCredentialsDto {
    code: Option<String>,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

/// Fetches temporary IAM role credentials via EC2 `IMDSv2` with in-memory caching.
#[derive(Debug)]
pub struct ImdsAwsCredentials {
    client: Client,
    metadata_base_url: String,
    clock: Arc<dyn Clock>,
    refresh_buffer_seconds: u64,
    cached: RwLock<Option<CachedCredentials>>,
}

impl ImdsAwsCredentials {
    /// Creates an `IMDSv2` credential source targeting the standard EC2 metadata endpoint.
    pub fn new(client: Client) -> Self {
        Self::with_base_url(client, DEFAULT_IMDS_BASE_URL)
    }

    /// Creates an `IMDSv2` credential source with a custom base URL (useful for local tests).
    pub fn with_base_url(client: Client, metadata_base_url: impl Into<String>) -> Self {
        Self::with_clock(client, metadata_base_url, Arc::new(SystemClock))
    }

    /// Creates an `IMDSv2` credential source with a custom base URL and injectable clock.
    pub fn with_clock(
        client: Client,
        metadata_base_url: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let base = metadata_base_url.into().trim_end_matches('/').to_owned();
        Self {
            client,
            metadata_base_url: base,
            clock,
            refresh_buffer_seconds: REFRESH_BUFFER_SECONDS,
            cached: RwLock::new(None),
        }
    }

    async fn fetch_imds_token(&self) -> Result<String, ProviderError> {
        let url = format!("{}/latest/api/token", self.metadata_base_url);
        let resp = self
            .client
            .put(&url)
            .header(IMDS_TOKEN_TTL_HEADER, DEFAULT_TOKEN_TTL_SECONDS)
            .send()
            .await
            .map_err(|err| map_transport_error(&err))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|_| ProviderError::Transient("failed reading IMDSv2 token".to_owned()))?;
        check_imds_status(status)?;

        let token = body.trim();
        if token.is_empty() {
            return Err(ProviderError::AuthFailed);
        }
        Ok(token.to_owned())
    }

    async fn fetch_role_name(&self, token: &str) -> Result<String, ProviderError> {
        let url = format!(
            "{}/latest/meta-data/iam/security-credentials/",
            self.metadata_base_url
        );
        let resp = self
            .client
            .get(&url)
            .header(IMDS_TOKEN_HEADER, token)
            .send()
            .await
            .map_err(|err| map_transport_error(&err))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|_| ProviderError::Transient("failed reading IMDSv2 role".to_owned()))?;
        check_imds_status(status)?;

        let role = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or(ProviderError::AuthFailed)?;

        if !is_safe_role_name(role) {
            return Err(ProviderError::AuthFailed);
        }
        Ok(role.to_owned())
    }

    async fn fetch_credentials(&self) -> Result<CachedCredentials, ProviderError> {
        let token = self.fetch_imds_token().await?;
        let role_name = self.fetch_role_name(&token).await?;
        let url = format!(
            "{}/latest/meta-data/iam/security-credentials/{role_name}",
            self.metadata_base_url
        );
        let resp = self
            .client
            .get(&url)
            .header(IMDS_TOKEN_HEADER, &token)
            .send()
            .await
            .map_err(|err| map_transport_error(&err))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(map_imds_status_error(status));
        }

        let dto: ImdsRoleCredentialsDto = resp
            .json()
            .await
            .map_err(|_| ProviderError::Transient("invalid IMDSv2 credentials json".to_owned()))?;

        if dto.code.as_deref().is_some_and(|code| code != "Success") {
            return Err(ProviderError::AuthFailed);
        }
        if dto.access_key_id.is_empty() || dto.secret_access_key.is_empty() || dto.token.is_empty()
        {
            return Err(ProviderError::AuthFailed);
        }

        let expires_at_unix = parse_rfc3339_unix(&dto.expiration)?;
        Ok(CachedCredentials {
            creds: AwsCredentials {
                access_key_id: dto.access_key_id,
                secret_access_key: SecretString::new(dto.secret_access_key),
                session_token: Some(SecretString::new(dto.token)),
            },
            expires_at_unix,
        })
    }
}

#[async_trait]
impl crate::credentials::AwsCredentialsSource for ImdsAwsCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        let now = self.clock.now_unix();
        {
            let guard = self.cached.read().await;
            if let Some(entry) = guard.as_ref()
                && entry.is_valid(now, self.refresh_buffer_seconds)
            {
                return Ok(entry.creds.clone());
            }
        }

        let mut guard = self.cached.write().await;
        let now = self.clock.now_unix();
        if let Some(entry) = guard.as_ref()
            && entry.is_valid(now, self.refresh_buffer_seconds)
        {
            return Ok(entry.creds.clone());
        }

        let refreshed = self.fetch_credentials().await?;
        let creds = refreshed.creds.clone();
        *guard = Some(refreshed);
        Ok(creds)
    }
}

fn is_safe_role_name(role: &str) -> bool {
    !role.is_empty()
        && role.len() <= 128
        && role != "."
        && role != ".."
        && role.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'=' | b',' | b'@')
        })
}

fn map_transport_error(err: &reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transient("aws imds transport error".to_owned())
    }
}

fn check_imds_status(status: StatusCode) -> Result<(), ProviderError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(map_imds_status_error(status))
    }
}

fn map_imds_status_error(status: StatusCode) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            ProviderError::AuthFailed
        }
        _ => ProviderError::Transient(format!("aws imds http {}", status.as_u16())),
    }
}
