//! GCP IAM Credentials API service account impersonation token source.

use crate::auth::{
    CachedToken, GcpTokenSource, REFRESH_BUFFER_SECONDS, is_safe_service_account,
    map_http_status_error, map_transport_error, parse_rfc3339_unix,
};
use async_trait::async_trait;
use open_ci_core::{Clock, SecretString, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default GCP IAM Credentials API base URL.
pub const DEFAULT_IAM_CREDENTIALS_BASE_URL: &str = "https://iamcredentials.googleapis.com";
/// Default Compute Engine `OAuth2` scope for impersonated runner management tokens.
pub const DEFAULT_COMPUTE_SCOPE: &str = "https://www.googleapis.com/auth/compute";
/// Default impersonated access token lifetime in seconds (1 hour).
pub const DEFAULT_IMPERSONATION_LIFETIME_SECONDS: u32 = 3600;

/// Configuration for cross-project GCP Service Account impersonation via IAM Credentials API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpersonationConfig {
    pub target_service_account: String,
    pub scopes: Vec<String>,
    pub lifetime_seconds: u32,
    pub iam_credentials_base_url: String,
}

impl ImpersonationConfig {
    /// Creates an impersonation configuration targeting `target_service_account` with Compute scope.
    pub fn new(target_service_account: impl Into<String>) -> Self {
        Self {
            target_service_account: target_service_account.into(),
            scopes: vec![DEFAULT_COMPUTE_SCOPE.to_owned()],
            lifetime_seconds: DEFAULT_IMPERSONATION_LIFETIME_SECONDS,
            iam_credentials_base_url: DEFAULT_IAM_CREDENTIALS_BASE_URL.to_owned(),
        }
    }

    /// Overrides the `OAuth2` scopes requested for the impersonated access token.
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Overrides the requested token lifetime in seconds.
    pub fn with_lifetime_seconds(mut self, lifetime_seconds: u32) -> Self {
        self.lifetime_seconds = lifetime_seconds;
        self
    }

    /// Overrides the IAM Credentials API base URL (useful for local tests).
    pub fn with_base_url(mut self, iam_credentials_base_url: impl Into<String>) -> Self {
        self.iam_credentials_base_url = iam_credentials_base_url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct GenerateAccessTokenRequest<'a> {
    scope: &'a [String],
    lifetime: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateAccessTokenResponse {
    access_token: String,
    expire_time: String,
}

/// Wraps a base [`GcpTokenSource`] and exchanges it for a short-lived impersonated access token.
#[derive(Debug)]
pub struct ImpersonatedTokenSource {
    base: Arc<dyn GcpTokenSource>,
    client: Client,
    config: ImpersonationConfig,
    clock: Arc<dyn Clock>,
    refresh_buffer_seconds: u64,
    cached: RwLock<Option<CachedToken>>,
}

impl ImpersonatedTokenSource {
    /// Creates an impersonated token source backed by `SystemClock`.
    pub fn new(base: Arc<dyn GcpTokenSource>, client: Client, config: ImpersonationConfig) -> Self {
        Self::with_clock(base, client, config, Arc::new(SystemClock))
    }

    /// Creates an impersonated token source with an injectable clock for deterministic tests.
    pub fn with_clock(
        base: Arc<dyn GcpTokenSource>,
        client: Client,
        mut config: ImpersonationConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        while config.iam_credentials_base_url.ends_with('/') {
            config.iam_credentials_base_url.pop();
        }
        Self {
            base,
            client,
            config,
            clock,
            refresh_buffer_seconds: REFRESH_BUFFER_SECONDS,
            cached: RwLock::new(None),
        }
    }

    fn validate_config(&self) -> Result<(), ProviderError> {
        if !is_safe_service_account(&self.config.target_service_account) {
            return Err(ProviderError::InvalidConfig(
                "invalid gcp target_service_account".to_owned(),
            ));
        }
        if self.config.scopes.is_empty() {
            return Err(ProviderError::InvalidConfig(
                "gcp impersonation scopes must not be empty".to_owned(),
            ));
        }
        if self.config.lifetime_seconds == 0 {
            return Err(ProviderError::InvalidConfig(
                "gcp impersonation lifetime_seconds must be positive".to_owned(),
            ));
        }
        Ok(())
    }

    async fn generate_token(&self) -> Result<CachedToken, ProviderError> {
        self.validate_config()?;
        let base_token = self.base.bearer_token().await?;

        let url = format!(
            "{}/v1/projects/-/serviceAccounts/{}:generateAccessToken",
            self.config.iam_credentials_base_url, self.config.target_service_account
        );
        let body = GenerateAccessTokenRequest {
            scope: &self.config.scopes,
            lifetime: format!("{}s", self.config.lifetime_seconds),
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(base_token.expose())
            .json(&body)
            .send()
            .await
            .map_err(|err| map_transport_error(&err, "gcp iam credentials transport error"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(map_http_status_error(status, "gcp iam credentials"));
        }

        let payload: GenerateAccessTokenResponse = resp.json().await.map_err(|_| {
            ProviderError::Transient("invalid gcp generateAccessToken json".to_owned())
        })?;
        if payload.access_token.trim().is_empty() {
            return Err(ProviderError::AuthFailed);
        }

        let expires_at_unix = parse_rfc3339_unix(&payload.expire_time)?;
        Ok(CachedToken {
            token: SecretString::new(payload.access_token),
            expires_at_unix,
        })
    }
}

#[async_trait]
impl GcpTokenSource for ImpersonatedTokenSource {
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

        let refreshed = self.generate_token().await?;
        let token = refreshed.token.clone();
        *guard = Some(refreshed);
        Ok(token)
    }
}
