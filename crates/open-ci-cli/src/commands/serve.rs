//! Control-plane wiring, HTTP server, background reconciler, and graceful shutdown.

use anyhow::{Context, Result, anyhow};
use open_ci_core::config::AwsProviderConfig;
use open_ci_core::{Clock, OpenCiConfig, SecretString, SystemClock};
use open_ci_github::{FileGitHubToken, GitHubClient, OidcVerifier, StaticGitHubToken};
use open_ci_provider::ProviderRegistry;
use open_ci_provider_aws::{
    AssumeRoleConfig, AwsCredentialChain, AwsCredentialsSource, AwsProvider,
    GceWebIdentityCredentials, WebIdentityConfig,
};
use open_ci_provider_gcp::{
    FileTokenSource, GcpProvider, GcpTokenSource, ImpersonatedTokenSource, ImpersonationConfig,
    MetadataTokenSource,
};
use open_ci_provider_mock::MockProvider;
use open_ci_server::http::build_router;
use open_ci_server::{ControlPlane, ServerSecrets};
use open_ci_store::SqliteStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub fn load_config(path: &Path) -> Result<OpenCiConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading config {}", path.display()))?;
    OpenCiConfig::parse_toml(&raw).context("configuration validation failed")
}

pub fn build_http_client(timeout_seconds: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("open-ci/0.1")
        .build()
        .context("failed constructing HTTP client")
}

pub fn build_control_plane(cfg: OpenCiConfig, demo_mode: bool) -> Result<ControlPlane> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let store = Arc::new(SqliteStore::open(
        Path::new(&cfg.server.database_path),
        clock.now_unix(),
    )?);
    let http = build_http_client(cfg.server.request_timeout_seconds)?;
    let registry = build_provider_registry(&cfg, &http)?;
    let (gh_tokens, secrets) = load_runtime_secrets(demo_mode)?;

    let scope = cfg.github_scope();
    let github = GitHubClient::new(
        cfg.github.api_url.clone(),
        scope.clone(),
        http.clone(),
        gh_tokens,
    );
    let oidc = cfg
        .github
        .oidc_trust
        .clone()
        .map(|trust| OidcVerifier::new(cfg.github.oidc_issuer.clone(), trust, scope, http));

    Ok(ControlPlane::new(
        cfg, store, registry, github, oidc, clock, secrets,
    )?)
}

pub async fn serve_with_reconciler(cp: ControlPlane) -> Result<()> {
    let listen = cp.config.server.listen.clone();
    let tick_secs = cp.config.server.tick_seconds;
    let shutdown = CancellationToken::new();

    let bg_cp = cp.clone();
    let bg_token = shutdown.clone();
    let reconciler_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(tick_secs));
        loop {
            tokio::select! {
                () = bg_token.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(err) = bg_cp.reconcile_once().await {
                        tracing::error!(error = %err, "reconcile tick failed");
                    }
                }
            }
        }
    });

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("failed binding {listen}"))?;
    let router = build_router(cp);

    let signal_token = shutdown.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("shutdown signal received; draining gracefully");
            signal_token.cancel();
        })
        .await?;

    let _ = reconciler_task.await;
    Ok(())
}

fn build_provider_registry(cfg: &OpenCiConfig, http: &reqwest::Client) -> Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider::default()));

    if let Some(gcp_cfg) = cfg.gcp.clone() {
        let base_tokens: Arc<dyn GcpTokenSource> = match std::env::var("OPEN_CI_GCP_TOKEN_FILE") {
            Ok(path) if !path.trim().is_empty() => Arc::new(FileTokenSource::new(
                PathBuf::from(path),
                Duration::from_secs(60),
            )),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                Arc::new(MetadataTokenSource::new(http.clone()))
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(anyhow!("`OPEN_CI_GCP_TOKEN_FILE` is not valid UTF-8"));
            }
        };
        let tokens: Arc<dyn GcpTokenSource> = match &gcp_cfg.impersonate_service_account {
            Some(target_sa) => Arc::new(ImpersonatedTokenSource::new(
                base_tokens,
                http.clone(),
                ImpersonationConfig::new(target_sa.clone()),
            )),
            None => base_tokens,
        };
        registry.register(Arc::new(GcpProvider::new(gcp_cfg, http.clone(), tokens)));
    }

    if let Some(aws_cfg) = cfg.aws.clone() {
        let creds = build_aws_credentials(&aws_cfg, http)?;
        registry.register(Arc::new(AwsProvider::new(aws_cfg, http.clone(), creds)));
    }

    Ok(registry)
}

fn build_aws_credentials(
    aws_cfg: &AwsProviderConfig,
    http: &reqwest::Client,
) -> Result<Arc<dyn AwsCredentialsSource>> {
    let session_name = aws_cfg
        .role_session_name
        .clone()
        .unwrap_or_else(|| "open-ci".to_owned());
    match (&aws_cfg.web_identity_audience, &aws_cfg.role_arn) {
        (Some(_), Some(_)) if aws_cfg.external_id.is_some() => Err(anyhow!(
            "`aws.external_id` cannot be combined with `aws.web_identity_audience`"
        )),
        (Some(audience), Some(arn)) => Ok(Arc::new(GceWebIdentityCredentials::new(
            http.clone(),
            WebIdentityConfig::new(arn.clone(), session_name, audience.clone()),
        ))),
        (Some(_), None) => Err(anyhow!(
            "`aws.web_identity_audience` requires `aws.role_arn`"
        )),
        (None, role_arn) => {
            let assume_role = role_arn.as_ref().map(|arn| {
                let role_cfg = AssumeRoleConfig::new(arn.clone(), session_name);
                match &aws_cfg.external_id {
                    Some(ext_id) => role_cfg.with_external_id(ext_id.clone()),
                    None => role_cfg,
                }
            });
            Ok(Arc::new(AwsCredentialChain::new(http.clone(), assume_role)))
        }
    }
}

fn load_runtime_secrets(
    demo_mode: bool,
) -> Result<(Arc<dyn open_ci_github::GitHubTokenSource>, ServerSecrets)> {
    if demo_mode {
        return Ok((
            Arc::new(StaticGitHubToken(SecretString::new("demo-github-token"))),
            ServerSecrets {
                webhook_secret: SecretString::new("demo-webhook-secret-0000000000000000"),
                bootstrap_secret: SecretString::new("demo-bootstrap-secret-00000000000000"),
                operator_token: Some(SecretString::new("demo-operator-token-000000000000")),
            },
        ));
    }

    let gh_token_path = std::env::var("OPEN_CI_GITHUB_TOKEN_FILE")
        .map_err(|_| anyhow!("set `OPEN_CI_GITHUB_TOKEN_FILE`"))?;
    let operator_token = read_optional_secret_file("OPEN_CI_OPERATOR_TOKEN_FILE")?;

    Ok((
        Arc::new(FileGitHubToken::new(
            PathBuf::from(gh_token_path),
            Duration::from_secs(60),
        )),
        ServerSecrets {
            webhook_secret: read_secret_file("OPEN_CI_WEBHOOK_SECRET_FILE")?,
            bootstrap_secret: read_secret_file("OPEN_CI_BOOTSTRAP_SECRET_FILE")?,
            operator_token,
        },
    ))
}

fn read_secret_file(env_var: &str) -> Result<SecretString> {
    let path = std::env::var(env_var)
        .map_err(|_| anyhow!("set `{env_var}` to a private credential file path"))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("cannot read secret file referenced by `{env_var}`"))?;
    Ok(SecretString::new(raw.trim()))
}

pub(crate) fn read_optional_secret_file(env_var: &str) -> Result<Option<SecretString>> {
    match std::env::var(env_var) {
        Ok(path) => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|_| anyhow!("cannot read secret file referenced by `{env_var}`"))?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("`{env_var}` is empty"));
            }
            Ok(Some(SecretString::new(trimmed)))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(anyhow!("`{env_var}` is not valid UTF-8")),
    }
}

pub(crate) async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aws_config(audience: Option<&str>, role_arn: Option<&str>) -> AwsProviderConfig {
        AwsProviderConfig {
            account_id: "123456789012".to_owned(),
            endpoint_url: None,
            role_arn: role_arn.map(str::to_owned),
            external_id: None,
            role_session_name: None,
            web_identity_audience: audience.map(str::to_owned),
        }
    }

    #[test]
    fn build_aws_credentials_fails_closed_on_invalid_web_identity_combinations() {
        let http = reqwest::Client::new();
        let orphan_audience = aws_config(Some("open-ci-aws-federation"), None);
        assert!(build_aws_credentials(&orphan_audience, &http).is_err());
        let federated = aws_config(
            Some("open-ci-aws-federation"),
            Some("arn:aws:iam::123456789012:role/open-ci-gcp-controller"),
        );
        assert!(build_aws_credentials(&federated, &http).is_ok());
        assert!(build_aws_credentials(&aws_config(None, None), &http).is_ok());
        let with_external_id = AwsProviderConfig {
            external_id: Some("tenant".to_owned()),
            ..federated
        };
        assert!(build_aws_credentials(&with_external_id, &http).is_err());
    }
}
