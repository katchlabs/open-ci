//! Keyless AWS credentials for controllers running outside AWS.
//!
//! Exchanges the host's Google Compute Engine identity token for temporary
//! credentials via the unsigned STS `AssumeRoleWithWebIdentity` call, so a
//! GCP-hosted controller never stores long-lived AWS access keys.

use crate::credentials::sts::read_sts_credentials;
use crate::credentials::{
    AwsCredentialsSource, CachedCredentials, CredentialCache, DEFAULT_ASSUME_ROLE_DURATION_SECONDS,
    DEFAULT_STS_ENDPOINT, map_transport_error,
};
use crate::sigv4::{self, AwsCredentials};
use async_trait::async_trait;
use open_ci_core::{Clock, SecretString, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::{Client, StatusCode, Url};
use std::sync::Arc;

/// Default GCE metadata server base URL used to mint identity tokens.
pub const DEFAULT_GCE_METADATA_BASE_URL: &str = "http://metadata.google.internal";
const MAX_IDENTITY_TOKEN_BYTES: usize = 16 * 1024;
const IDENTITY_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/identity";

/// Configuration for STS `AssumeRoleWithWebIdentity` using a GCE identity token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebIdentityConfig {
    pub role_arn: String,
    pub role_session_name: String,
    /// Audience requested from the GCE metadata server; the role's trust policy pins it.
    pub audience: String,
    pub duration_seconds: u32,
    pub sts_endpoint: String,
    pub metadata_base_url: String,
}

impl WebIdentityConfig {
    /// Creates a config with a one-hour session, the global STS endpoint, and GCE metadata.
    pub fn new(
        role_arn: impl Into<String>,
        role_session_name: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        Self {
            role_arn: role_arn.into(),
            role_session_name: role_session_name.into(),
            audience: audience.into(),
            duration_seconds: DEFAULT_ASSUME_ROLE_DURATION_SECONDS,
            sts_endpoint: DEFAULT_STS_ENDPOINT.to_owned(),
            metadata_base_url: DEFAULT_GCE_METADATA_BASE_URL.to_owned(),
        }
    }

    /// Sets the requested session duration in seconds.
    pub fn with_duration_seconds(mut self, duration_seconds: u32) -> Self {
        self.duration_seconds = duration_seconds;
        self
    }

    /// Overrides the STS endpoint URL (regional endpoint or loopback test mock).
    pub fn with_sts_endpoint(mut self, sts_endpoint: impl Into<String>) -> Self {
        self.sts_endpoint = sts_endpoint.into();
        self
    }

    /// Overrides the GCE metadata server base URL (loopback test mock).
    pub fn with_metadata_base_url(mut self, metadata_base_url: impl Into<String>) -> Self {
        metadata_base_url
            .into()
            .trim_end_matches('/')
            .clone_into(&mut self.metadata_base_url);
        self
    }

    fn validate(&self) -> Result<(), ProviderError> {
        let required = [
            ("role_arn", &self.role_arn),
            ("role_session_name", &self.role_session_name),
            ("audience", &self.audience),
        ];
        if let Some((field, _)) = required.iter().find(|(_, v)| v.trim().is_empty()) {
            return Err(ProviderError::InvalidConfig(format!(
                "web identity {field} must not be empty"
            )));
        }
        if self.duration_seconds == 0 {
            return Err(ProviderError::InvalidConfig(
                "web identity duration_seconds must be positive".to_owned(),
            ));
        }
        for (field, url) in [
            ("sts_endpoint", &self.sts_endpoint),
            ("metadata_base_url", &self.metadata_base_url),
        ] {
            if !is_http_url(url) {
                return Err(ProviderError::InvalidConfig(format!(
                    "web identity {field} must be an http(s) URL with a host"
                )));
            }
        }
        Ok(())
    }

    fn identity_url(&self) -> Result<Url, ProviderError> {
        let base = self.metadata_base_url.trim_end_matches('/');
        let mut url = Url::parse(&format!("{base}{IDENTITY_PATH}"))
            .map_err(|_| ProviderError::InvalidConfig("invalid gce metadata url".to_owned()))?;
        url.query_pairs_mut()
            .append_pair("audience", &self.audience)
            .append_pair("format", "full");
        Ok(url)
    }
}

/// AWS credentials obtained by federating the host's GCE identity into an IAM role.
#[derive(Debug)]
pub struct GceWebIdentityCredentials {
    client: Client,
    config: WebIdentityConfig,
    cache: CredentialCache,
}

impl GceWebIdentityCredentials {
    /// Creates a web identity credential source backed by `SystemClock`.
    pub fn new(client: Client, config: WebIdentityConfig) -> Self {
        Self::with_clock(client, config, Arc::new(SystemClock))
    }

    /// Creates a web identity credential source with an injectable clock for testing.
    pub fn with_clock(client: Client, config: WebIdentityConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            client,
            config,
            cache: CredentialCache::new(clock),
        }
    }

    async fn exchange(&self) -> Result<CachedCredentials, ProviderError> {
        self.config.validate()?;
        let token = self.fetch_identity_token().await?;
        let params = [
            ("Action", "AssumeRoleWithWebIdentity".to_owned()),
            ("Version", "2011-06-15".to_owned()),
            ("RoleArn", self.config.role_arn.clone()),
            ("RoleSessionName", self.config.role_session_name.clone()),
            ("DurationSeconds", self.config.duration_seconds.to_string()),
            ("WebIdentityToken", token.expose().to_owned()),
        ];
        let sent = self
            .client
            .post(&self.config.sts_endpoint)
            .header(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )
            .body(sigv4::form_encode(&params))
            .send()
            .await;
        read_sts_credentials(sent, "AssumeRoleWithWebIdentityResponse").await
    }

    async fn fetch_identity_token(&self) -> Result<SecretString, ProviderError> {
        let resp = self
            .client
            .get(self.config.identity_url()?)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|err| map_transport_error(&err, "gce identity"))?;
        check_identity_status(resp.status())?;
        if resp
            .headers()
            .get("metadata-flavor")
            .is_none_or(|v| v.as_bytes() != b"Google")
        {
            return Err(ProviderError::Transient(
                "identity response is not from the GCE metadata server".to_owned(),
            ));
        }
        let body = resp.text().await.map_err(|_| {
            ProviderError::Transient("failed reading gce identity token".to_owned())
        })?;
        parse_identity_token(&body)
    }
}

#[async_trait]
impl AwsCredentialsSource for GceWebIdentityCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        self.cache.get_or_refresh(|| self.exchange()).await
    }
}

fn check_identity_status(status: StatusCode) -> Result<(), ProviderError> {
    match status {
        s if s.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            Err(ProviderError::AuthFailed)
        }
        s => Err(ProviderError::Transient(format!(
            "gce identity http {}",
            s.as_u16()
        ))),
    }
}

/// Accepts only a compact JWS: three non-empty base64url segments.
fn parse_identity_token(body: &str) -> Result<SecretString, ProviderError> {
    let token = body.trim();
    let is_segment = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    let well_formed = token.len() <= MAX_IDENTITY_TOKEN_BYTES
        && token.split('.').count() == 3
        && token.split('.').all(is_segment);
    if well_formed {
        Ok(SecretString::new(token))
    } else {
        Err(ProviderError::Transient(
            "malformed gce identity token".to_owned(),
        ))
    }
}

fn is_http_url(raw: &str) -> bool {
    Url::parse(raw)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}
