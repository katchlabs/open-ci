//! In-memory multi-tenant GitHub App installation token minter with singleflight refresh.

use crate::client::{GitHubError, GitHubTokenSource};
use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use open_ci_core::{Clock, SecretString, SystemClock};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const DEFAULT_TOKEN_TTL_SECS: u64 = 45 * 60;
const MAX_TOKEN_TTL_SECS: u64 = 55 * 60;
const REFRESH_BUFFER_SECS: u64 = 120;

/// Signs short-lived RS256 GitHub App JWTs (backed by PEM in memory or Cloud KMS HSM).
#[async_trait]
pub trait AppJwtSigner: Send + Sync + std::fmt::Debug {
    async fn sign_app_jwt(&self, app_id: u64, now_unix: u64) -> Result<String, GitHubError>;
}

#[derive(Serialize)]
struct AppJwtClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

/// Local RSA PEM implementation of [`AppJwtSigner`].
pub struct RsaPemJwtSigner {
    key: EncodingKey,
}

impl std::fmt::Debug for RsaPemJwtSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaPemJwtSigner").finish_non_exhaustive()
    }
}

impl RsaPemJwtSigner {
    pub fn from_pem(pem: &SecretString) -> Result<Self, GitHubError> {
        let key = EncodingKey::from_rsa_pem(pem.expose().as_bytes())
            .map_err(|_| GitHubError::AuthFailed)?;
        Ok(Self { key })
    }
}

#[async_trait]
impl AppJwtSigner for RsaPemJwtSigner {
    async fn sign_app_jwt(&self, app_id: u64, now_unix: u64) -> Result<String, GitHubError> {
        encode(
            &Header::new(Algorithm::RS256),
            &AppJwtClaims {
                iat: now_unix.saturating_sub(30),
                exp: now_unix.saturating_add(540),
                iss: app_id.to_string(),
            },
            &self.key,
        )
        .map_err(|_| GitHubError::AuthFailed)
    }
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Clone, Debug)]
struct CachedToken {
    token: SecretString,
    refresh_after_unix: u64,
}

#[derive(Debug, Default)]
struct InstallationSlot {
    refresh_lock: Mutex<()>,
    cached: RwLock<Option<CachedToken>>,
}

/// Multi-tenant GitHub App installation token minter caching one token per `installation_id`.
#[derive(Debug)]
pub struct GitHubAppTokenMinter {
    app_id: u64,
    api_url: String,
    client: Client,
    signer: Arc<dyn AppJwtSigner>,
    clock: Arc<dyn Clock>,
    slots: RwLock<HashMap<u64, Arc<InstallationSlot>>>,
}

impl GitHubAppTokenMinter {
    pub fn new(
        app_id: u64,
        api_url: &str,
        client: Client,
        signer: Arc<dyn AppJwtSigner>,
    ) -> Result<Self, GitHubError> {
        Self::with_clock(app_id, api_url, client, signer, Arc::new(SystemClock))
    }

    pub fn with_clock(
        app_id: u64,
        api_url: &str,
        client: Client,
        signer: Arc<dyn AppJwtSigner>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, GitHubError> {
        if app_id == 0 {
            return Err(GitHubError::Api("github app_id must be > 0".to_owned()));
        }
        validate_https_or_loopback(api_url)?;
        Ok(Self {
            app_id,
            api_url: api_url.trim_end_matches('/').to_owned(),
            client,
            signer,
            clock,
            slots: RwLock::new(HashMap::new()),
        })
    }

    /// Returns a [`GitHubTokenSource`] bound to `installation_id` backed by this minter's cache.
    pub fn token_source(self: &Arc<Self>, installation_id: u64) -> Arc<dyn GitHubTokenSource> {
        Arc::new(InstallationTokenSource {
            minter: Arc::clone(self),
            installation_id,
        })
    }

    /// Evicts any cached token for `installation_id` (for example after suspension or 401).
    pub async fn invalidate(&self, installation_id: u64) {
        let slot = { self.slots.read().await.get(&installation_id).cloned() };
        if let Some(slot) = slot {
            *slot.cached.write().await = None;
        }
    }

    /// Returns a cached or freshly minted installation token for `installation_id`.
    pub async fn installation_token(
        &self,
        installation_id: u64,
    ) -> Result<SecretString, GitHubError> {
        if installation_id == 0 {
            return Err(GitHubError::AuthFailed);
        }

        let slot = self.slot_for(installation_id).await;
        let now = self.clock.now_unix();
        if let Some(tok) = Self::read_fresh(&slot, now).await {
            return Ok(tok);
        }

        let _refresh_guard = slot.refresh_lock.lock().await;
        let now = self.clock.now_unix();
        if let Some(tok) = Self::read_fresh(&slot, now).await {
            return Ok(tok);
        }

        let minted = self.mint_installation_token(installation_id, now).await?;
        let token = minted.token.clone();
        *slot.cached.write().await = Some(minted);
        Ok(token)
    }

    async fn slot_for(&self, installation_id: u64) -> Arc<InstallationSlot> {
        if let Some(existing) = self.slots.read().await.get(&installation_id) {
            return Arc::clone(existing);
        }
        let mut guard = self.slots.write().await;
        Arc::clone(
            guard
                .entry(installation_id)
                .or_insert_with(|| Arc::new(InstallationSlot::default())),
        )
    }

    async fn read_fresh(slot: &InstallationSlot, now_unix: u64) -> Option<SecretString> {
        let guard = slot.cached.read().await;
        guard
            .as_ref()
            .filter(|entry| now_unix < entry.refresh_after_unix)
            .map(|entry| entry.token.clone())
    }

    async fn mint_installation_token(
        &self,
        installation_id: u64,
        now_unix: u64,
    ) -> Result<CachedToken, GitHubError> {
        let jwt = self.signer.sign_app_jwt(self.app_id, now_unix).await?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_url
        );

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    GitHubError::Timeout
                } else {
                    GitHubError::Api("installation token exchange failed".to_owned())
                }
            })?;

        match resp.status() {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(GitHubError::AuthFailed);
            }
            StatusCode::NOT_FOUND => return Err(GitHubError::NotFound),
            StatusCode::TOO_MANY_REQUESTS => return Err(GitHubError::RateLimited),
            status => {
                return Err(GitHubError::Api(format!(
                    "installation token http {}",
                    status.as_u16()
                )));
            }
        }

        let parsed: InstallationTokenResponse = resp
            .json()
            .await
            .map_err(|_| GitHubError::Api("invalid installation token payload".to_owned()))?;
        let trimmed = parsed.token.trim();
        if trimmed.is_empty() {
            return Err(GitHubError::AuthFailed);
        }

        let refresh_after_unix = compute_refresh_deadline(now_unix, parsed.expires_at.as_deref());
        Ok(CachedToken {
            token: SecretString::new(trimmed),
            refresh_after_unix,
        })
    }
}

#[derive(Debug)]
struct InstallationTokenSource {
    minter: Arc<GitHubAppTokenMinter>,
    installation_id: u64,
}

#[async_trait]
impl GitHubTokenSource for InstallationTokenSource {
    async fn bearer_token(&self) -> Result<SecretString, GitHubError> {
        self.minter.installation_token(self.installation_id).await
    }
}

fn compute_refresh_deadline(now_unix: u64, expires_at: Option<&str>) -> u64 {
    let default_deadline = now_unix.saturating_add(DEFAULT_TOKEN_TTL_SECS);
    let max_deadline = now_unix.saturating_add(MAX_TOKEN_TTL_SECS);
    let Some(exp) = expires_at.and_then(parse_rfc3339_utc) else {
        return default_deadline;
    };
    if exp <= now_unix {
        return now_unix;
    }
    exp.saturating_sub(REFRESH_BUFFER_SECS)
        .max(now_unix)
        .min(max_deadline)
}

fn parse_rfc3339_utc(input: &str) -> Option<u64> {
    let trimmed = input.trim();
    let core = trimmed.strip_suffix('Z')?;
    let (date_part, time_part) = core.split_once('T')?;
    let time_whole = time_part.split('.').next()?;

    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next()?.parse().ok()?;
    let month: u32 = date_iter.next()?.parse().ok()?;
    let day: u32 = date_iter.next()?.parse().ok()?;
    if date_iter.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut time_iter = time_whole.split(':');
    let hour: u64 = time_iter.next()?.parse().ok()?;
    let minute: u64 = time_iter.next()?.parse().ok()?;
    let second: u64 = time_iter.next()?.parse().ok()?;
    if time_iter.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let days = days_from_civil(year, month, day)?;
    let secs = u64::try_from(days).ok()?.checked_mul(86_400)?;
    secs.checked_add(hour * 3_600 + minute * 60 + second)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let day_of_year = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + day_of_year;
    let days = era * 146_097 + doe - 719_468;
    (days >= 0).then_some(days)
}

fn validate_https_or_loopback(url: &str) -> Result<(), GitHubError> {
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
    Err(GitHubError::Api(
        "github_api_url must use https:// (or http:// loopback for tests)".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_rfc3339_utc_timestamps() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc("2023-11-14T22:13:20Z"),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_rfc3339_utc("2023-11-14T22:13:20.123Z"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_rfc3339_utc("not-a-timestamp"), None);
    }
}
