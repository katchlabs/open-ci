//! AWS STS `AssumeRole` credential provider using native `SigV4` signing and XML parsing.

use crate::credentials::{
    AwsCredentialsSource, CachedCredentials, CredentialCache, map_transport_error,
    parse_rfc3339_unix,
};
use crate::sigv4::{self, AwsCredentials};
use crate::xml;
use async_trait::async_trait;
use open_ci_core::{Clock, SystemClock};
use open_ci_provider::ProviderError;
use reqwest::{Client, StatusCode};
use std::sync::Arc;

/// Default global AWS STS endpoint.
pub const DEFAULT_STS_ENDPOINT: &str = "https://sts.amazonaws.com/";
/// Default `AssumeRole` session duration in seconds (1 hour).
pub const DEFAULT_ASSUME_ROLE_DURATION_SECONDS: u32 = 3600;
const DEFAULT_STS_SIGNING_REGION: &str = "us-east-1";

/// Configuration for AWS STS `AssumeRole` cross-account / least-privilege session exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssumeRoleConfig {
    pub role_arn: String,
    pub role_session_name: String,
    pub external_id: Option<String>,
    pub duration_seconds: u32,
    pub sts_endpoint: String,
}

impl AssumeRoleConfig {
    /// Creates an `AssumeRoleConfig` with default 3600s duration and global STS endpoint.
    pub fn new(role_arn: impl Into<String>, role_session_name: impl Into<String>) -> Self {
        Self {
            role_arn: role_arn.into(),
            role_session_name: role_session_name.into(),
            external_id: None,
            duration_seconds: DEFAULT_ASSUME_ROLE_DURATION_SECONDS,
            sts_endpoint: DEFAULT_STS_ENDPOINT.to_owned(),
        }
    }

    /// Sets an `ExternalId` for cross-account confused-deputy protection.
    pub fn with_external_id(mut self, external_id: impl Into<String>) -> Self {
        self.external_id = Some(external_id.into());
        self
    }

    /// Sets the requested session duration in seconds.
    pub fn with_duration_seconds(mut self, duration_seconds: u32) -> Self {
        self.duration_seconds = duration_seconds;
        self
    }

    /// Overrides the STS endpoint URL (e.g., regional endpoint or loopback test mock).
    pub fn with_sts_endpoint(mut self, sts_endpoint: impl Into<String>) -> Self {
        self.sts_endpoint = sts_endpoint.into();
        self
    }

    /// Configures a regional AWS STS endpoint (`https://sts.<region>.amazonaws.com/`).
    pub fn with_region(mut self, region: &str) -> Self {
        self.sts_endpoint = format!("https://sts.{region}.amazonaws.com/");
        self
    }
}

/// Wraps a base AWS credential source and assumes a target IAM role via STS `AssumeRole`.
#[derive(Debug)]
pub struct StsAssumeRoleCredentials {
    base: Arc<dyn AwsCredentialsSource>,
    client: Client,
    config: AssumeRoleConfig,
    clock: Arc<dyn Clock>,
    cache: CredentialCache,
}

impl StsAssumeRoleCredentials {
    /// Creates an STS `AssumeRole` credential source backed by `SystemClock`.
    pub fn new(
        base: Arc<dyn AwsCredentialsSource>,
        client: Client,
        config: AssumeRoleConfig,
    ) -> Self {
        Self::with_clock(base, client, config, Arc::new(SystemClock))
    }

    /// Creates an STS `AssumeRole` credential source with an injectable clock for testing.
    pub fn with_clock(
        base: Arc<dyn AwsCredentialsSource>,
        client: Client,
        config: AssumeRoleConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            base,
            client,
            config,
            cache: CredentialCache::new(Arc::clone(&clock)),
            clock,
        }
    }

    /// Creates a two-hop credential source for GCP-hosted control planes:
    /// GCE OIDC -> AWS hub role (`AssumeRoleWithWebIdentity`) -> target customer role (`AssumeRole` + `ExternalId`).
    pub fn over_gce_web_identity(
        hub_config: crate::credentials::WebIdentityConfig,
        target_config: AssumeRoleConfig,
        client: Client,
    ) -> Self {
        Self::over_gce_web_identity_with_clock(
            hub_config,
            target_config,
            client,
            Arc::new(SystemClock),
        )
    }

    /// Creates a two-hop GCE-to-AWS-hub-to-customer credential source with an injectable clock.
    pub fn over_gce_web_identity_with_clock(
        hub_config: crate::credentials::WebIdentityConfig,
        target_config: AssumeRoleConfig,
        client: Client,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let hub = Arc::new(crate::credentials::GceWebIdentityCredentials::with_clock(
            client.clone(),
            hub_config,
            Arc::clone(&clock),
        ));
        Self::with_clock(hub, client, target_config, clock)
    }

    fn validate_config(&self) -> Result<(), ProviderError> {
        if self.config.role_arn.trim().is_empty() {
            return Err(ProviderError::InvalidConfig(
                "sts role_arn must not be empty".to_owned(),
            ));
        }
        if self.config.role_session_name.trim().is_empty() {
            return Err(ProviderError::InvalidConfig(
                "sts role_session_name must not be empty".to_owned(),
            ));
        }
        if self.config.duration_seconds == 0 {
            return Err(ProviderError::InvalidConfig(
                "sts duration_seconds must be positive".to_owned(),
            ));
        }
        Ok(())
    }

    fn parse_endpoint(&self) -> Result<(String, String, String, String), ProviderError> {
        let parsed = reqwest::Url::parse(&self.config.sts_endpoint)
            .map_err(|_| ProviderError::InvalidConfig("invalid sts_endpoint url".to_owned()))?;
        let raw_host = parsed.host_str().ok_or_else(|| {
            ProviderError::InvalidConfig("missing host in sts_endpoint".to_owned())
        })?;
        let host = match parsed.port() {
            Some(port) => format!("{raw_host}:{port}"),
            None => raw_host.to_owned(),
        };
        let path = if parsed.path().is_empty() {
            "/".to_owned()
        } else {
            parsed.path().to_owned()
        };
        let signing_region = infer_sts_region(raw_host);
        Ok((self.config.sts_endpoint.clone(), host, path, signing_region))
    }

    async fn assume_role(&self) -> Result<CachedCredentials, ProviderError> {
        self.validate_config()?;
        let (url, host, path, region) = self.parse_endpoint()?;
        let base_creds = self.base.credentials().await?;

        let mut params = vec![
            ("Action", "AssumeRole".to_owned()),
            ("Version", "2011-06-15".to_owned()),
            ("RoleArn", self.config.role_arn.clone()),
            ("RoleSessionName", self.config.role_session_name.clone()),
            ("DurationSeconds", self.config.duration_seconds.to_string()),
        ];
        if let Some(ext_id) = &self.config.external_id
            && !ext_id.is_empty()
        {
            params.push(("ExternalId", ext_id.clone()));
        }

        let body = sigv4::form_encode(&params);
        let signed = sigv4::sign_post(
            &base_creds,
            &region,
            "sts",
            &host,
            &path,
            &body,
            self.clock.now_unix(),
        )
        .map_err(ProviderError::Transient)?;

        let mut req = self
            .client
            .post(&url)
            .header(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization)
            .body(body);

        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token.expose());
        }

        read_sts_credentials(req.send().await, "AssumeRoleResponse").await
    }
}

#[async_trait]
impl AwsCredentialsSource for StsAssumeRoleCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        self.cache.get_or_refresh(|| self.assume_role()).await
    }
}

fn infer_sts_region(host: &str) -> String {
    let lower = host.to_ascii_lowercase();
    let stripped = lower.strip_prefix("sts.");
    if let Some(rest) = stripped
        && let Some(region) = rest
            .strip_suffix(".amazonaws.com")
            .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))
        && !region.is_empty()
        && !region.contains('.')
    {
        return region.to_owned();
    }
    DEFAULT_STS_SIGNING_REGION.to_owned()
}

/// Reads an STS credential response, classifying transport, HTTP and XML errors.
pub(crate) async fn read_sts_credentials(
    sent: Result<reqwest::Response, reqwest::Error>,
    response_name: &str,
) -> Result<CachedCredentials, ProviderError> {
    let resp = sent.map_err(|err| map_transport_error(&err, "aws sts"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|_| ProviderError::Transient("failed reading aws sts response".to_owned()))?;
    if !status.is_success() {
        return Err(classify_sts_error(status, &text));
    }
    let parsed = xml::parse_assume_role_credentials(&text).ok_or_else(|| {
        ProviderError::Transient(format!("missing credentials in STS {response_name}"))
    })?;
    Ok(CachedCredentials {
        expires_at_unix: parse_rfc3339_unix(&parsed.expiration_rfc3339)?,
        creds: AwsCredentials {
            access_key_id: parsed.access_key_id,
            secret_access_key: parsed.secret_access_key,
            session_token: Some(parsed.session_token),
        },
    })
}

fn classify_sts_error(status: StatusCode, xml_body: &str) -> ProviderError {
    let code = xml::parse_error_code(xml_body).unwrap_or_default();
    match code.as_str() {
        "AccessDenied"
        | "AuthFailure"
        | "ExpiredToken"
        | "ExpiredTokenException"
        | "IDPRejectedClaim"
        | "InvalidClientTokenId"
        | "InvalidIdentityToken"
        | "SignatureDoesNotMatch" => ProviderError::AuthFailed,
        "Throttling" | "RequestLimitExceeded" => ProviderError::RateLimited {
            retry_after_seconds: 30,
        },
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            ProviderError::AuthFailed
        }
        _ => ProviderError::Transient(if code.is_empty() {
            format!("aws sts http {}", status.as_u16())
        } else {
            format!("aws sts error {code}")
        }),
    }
}
