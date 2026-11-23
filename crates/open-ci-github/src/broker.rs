//! Privilege-separated credential rotation broker for GitHub App installation tokens
//! and GCP metadata access tokens.

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use open_ci_core::SecretString;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("credential broker io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("jwt signing failed")]
    JwtSign,

    #[error("credential upstream request failed: {0}")]
    Upstream(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key_path: PathBuf,
    pub github_api_url: String,
    pub github_token_output: PathBuf,
    #[serde(default)]
    pub gcp_metadata_url: Option<String>,
    #[serde(default)]
    pub gcp_token_output: Option<PathBuf>,
    #[serde(default)]
    pub health_output: Option<PathBuf>,
}

#[derive(Serialize)]
struct AppJwtClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct MetadataTokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Rotates GitHub App installation token (and optional GCP metadata token) atomically.
pub async fn rotate_credentials(
    cfg: &BrokerConfig,
    client: &Client,
    now_unix: u64,
) -> Result<(), BrokerError> {
    validate_https_or_loopback(&cfg.github_api_url)?;
    let pem = read_private_key(&cfg.private_key_path)?;
    let key =
        EncodingKey::from_rsa_pem(pem.expose().as_bytes()).map_err(|_| BrokerError::JwtSign)?;
    let jwt = encode(
        &Header::new(Algorithm::RS256),
        &AppJwtClaims {
            iat: now_unix.saturating_sub(30),
            exp: now_unix.saturating_add(540),
            iss: cfg.app_id.to_string(),
        },
        &key,
    )
    .map_err(|_| BrokerError::JwtSign)?;

    let gh_url = format!(
        "{}/app/installations/{}/access_tokens",
        cfg.github_api_url.trim_end_matches('/'),
        cfg.installation_id
    );
    let gh_resp = client
        .post(&gh_url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| BrokerError::Upstream("github installation token exchange".to_owned()))?;
    if !gh_resp.status().is_success() {
        return Err(BrokerError::Upstream(format!(
            "github http {}",
            gh_resp.status().as_u16()
        )));
    }
    let gh_token: InstallationTokenResponse = gh_resp
        .json()
        .await
        .map_err(|_| BrokerError::Upstream("invalid github token json".to_owned()))?;
    atomic_write_secret(&cfg.github_token_output, &SecretString::new(gh_token.token))?;

    let mut gcp_expires_in = None;
    if let (Some(meta_url), Some(gcp_out)) = (&cfg.gcp_metadata_url, &cfg.gcp_token_output) {
        let gcp_resp = client
            .get(meta_url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|_| BrokerError::Upstream("gcp metadata token request".to_owned()))?;
        if !gcp_resp.status().is_success() {
            return Err(BrokerError::Upstream(format!(
                "gcp metadata http {}",
                gcp_resp.status().as_u16()
            )));
        }
        let parsed: MetadataTokenResponse = gcp_resp
            .json()
            .await
            .map_err(|_| BrokerError::Upstream("invalid gcp metadata json".to_owned()))?;
        gcp_expires_in = Some(parsed.expires_in);
        atomic_write_secret(gcp_out, &SecretString::new(parsed.access_token))?;
    }

    if let Some(health_out) = &cfg.health_output {
        let health = serde_json::json!({
            "rotated_at_unix": now_unix,
            "github_expires_at": gh_token.expires_at,
            "gcp_expires_in_seconds": gcp_expires_in,
        });
        atomic_write_bytes(health_out, health.to_string().as_bytes(), 0o644)?;
    }

    Ok(())
}

fn validate_https_or_loopback(url: &str) -> Result<(), BrokerError> {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        let host = if let Some(bracketed) = authority.strip_prefix('[') {
            bracketed.split(']').next().unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        if matches!(host, "127.0.0.1" | "localhost" | "::1") {
            return Ok(());
        }
    }
    Err(BrokerError::Upstream(
        "github_api_url must use https:// (or http:// loopback for tests)".to_owned(),
    ))
}

fn read_private_key(path: &Path) -> Result<SecretString, BrokerError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(BrokerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private key must be a regular non-symlink file",
        )));
    }
    #[cfg(unix)]
    if meta.mode() & 0o077 != 0 {
        return Err(BrokerError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private key file must not be group- or world-accessible (chmod 0600)",
        )));
    }
    let contents = std::fs::read_to_string(path)?;
    Ok(SecretString::new(contents))
}

fn atomic_write_secret(path: &Path, secret: &SecretString) -> Result<(), BrokerError> {
    // 0o640 lets privilege-separated User=open-ci-broker write tokens readable by Group=open-ci.
    atomic_write_bytes(path, format!("{}\n", secret.expose()).as_bytes(), 0o640)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8], mode: u32) -> Result<(), BrokerError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;

    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| BrokerError::Io(e.error))?;
    Ok(())
}
