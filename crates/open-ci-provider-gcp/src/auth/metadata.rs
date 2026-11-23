//! GCE/GKE Instance Metadata Server `OAuth2` bearer token source.

use crate::auth::{
    CachedToken, GcpTokenSource, REFRESH_BUFFER_SECONDS, is_safe_service_account,
    map_http_status_error, map_transport_error,
};
use async_trait::async_trait;
use open_ci_core::{Clock, SecretString, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Standard GCE/GKE metadata server base URL.
pub const DEFAULT_GCP_METADATA_BASE_URL: &str = "http://metadata.google.internal";
/// Default attached service account alias on GCE/GKE instances.
pub const DEFAULT_METADATA_ACCOUNT: &str = "default";

#[derive(Debug, Deserialize)]
struct MetadataTokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Fetches `OAuth2` access tokens from the GCE/GKE metadata server with expiry caching.
#[derive(Debug)]
pub struct MetadataTokenSource {
    client: Client,
    metadata_base_url: String,
    account: String,
    clock: Arc<dyn Clock>,
    refresh_buffer_seconds: u64,
    cached: RwLock<Option<CachedToken>>,
}

impl MetadataTokenSource {
    /// Creates a metadata token source for the `default` instance service account.
    pub fn new(client: Client) -> Self {
        Self::with_account(client, DEFAULT_METADATA_ACCOUNT)
    }

    /// Creates a metadata token source for a specific attached service account.
    pub fn with_account(client: Client, account: impl Into<String>) -> Self {
        Self::with_endpoint(client, DEFAULT_GCP_METADATA_BASE_URL, account)
    }

    /// Creates a metadata token source with a configurable base URL (useful for local tests).
    pub fn with_endpoint(
        client: Client,
        metadata_base_url: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self::with_clock(client, metadata_base_url, account, Arc::new(SystemClock))
    }

    /// Creates a metadata token source with a custom base URL and injectable clock.
    pub fn with_clock(
        client: Client,
        metadata_base_url: impl Into<String>,
        account: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            client,
            metadata_base_url: metadata_base_url.into().trim_end_matches('/').to_owned(),
            account: account.into(),
            clock,
            refresh_buffer_seconds: REFRESH_BUFFER_SECONDS,
            cached: RwLock::new(None),
        }
    }

    async fn fetch_token(&self) -> Result<CachedToken, ProviderError> {
        if !is_safe_service_account(&self.account) {
            return Err(ProviderError::InvalidConfig(
                "invalid gcp metadata service account".to_owned(),
            ));
        }

        let url = format!(
            "{}/computeMetadata/v1/instance/service-accounts/{}/token",
            self.metadata_base_url, self.account
        );
        let resp = self
            .client
            .get(&url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|err| map_transport_error(&err, "gcp metadata transport error"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(map_http_status_error(status, "gcp metadata"));
        }

        let payload: MetadataTokenResponse = resp
            .json()
            .await
            .map_err(|_| ProviderError::Transient("invalid gcp metadata token json".to_owned()))?;
        if payload.access_token.trim().is_empty() {
            return Err(ProviderError::AuthFailed);
        }

        let expires_at_unix = self.clock.now_unix().saturating_add(payload.expires_in);
        Ok(CachedToken {
            token: SecretString::new(payload.access_token),
            expires_at_unix,
        })
    }
}

#[async_trait]
impl GcpTokenSource for MetadataTokenSource {
    async fn bearer_token(&self) -> Result<SecretString, ProviderError> {
        let now = self.clock.now_unix();
        {
            let guard = self.cached.read().await;
            if let Some(entry) = guard.as_ref()
                && entry.is_valid(now, self.refresh_buffer_seconds)
            {
                return Ok(entry.token.clone());
            }
        }

        let mut guard = self.cached.write().await;
        let now = self.clock.now_unix();
        if let Some(entry) = guard.as_ref()
            && entry.is_valid(now, self.refresh_buffer_seconds)
        {
            return Ok(entry.token.clone());
        }

        let refreshed = self.fetch_token().await?;
        let token = refreshed.token.clone();
        *guard = Some(refreshed);
        Ok(token)
    }
}
