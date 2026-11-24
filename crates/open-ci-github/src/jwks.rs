//! Remote JWKS with TTL caching, singleflight refresh, and a refresh cooldown for unknown keys.

use jsonwebtoken::jwk::{Jwk, JwkSet};
use reqwest::Client;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

const MAX_JWKS_BYTES: usize = 64 * 1024;
const FORCED_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const CACHE_TTL: Duration = Duration::from_secs(300);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JwksError {
    #[error("failed fetching or parsing jwks")]
    Unavailable,

    #[error("no jwks key matches the token key id")]
    UnknownKey,
}

/// JWKS document fetched from a fixed URL and shared by concurrent verifications.
#[derive(Debug)]
pub struct RemoteJwks {
    url: String,
    client: Client,
    cache: RwLock<Option<(JwkSet, Instant)>>,
    refresh_lock: Mutex<()>,
}

impl RemoteJwks {
    pub fn new(url: String, client: Client) -> Self {
        Self {
            url,
            client,
            cache: RwLock::new(None),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Conventional GitHub location `{issuer}/.well-known/jwks`.
    pub fn for_issuer(issuer: &str, client: Client) -> Self {
        Self::new(
            format!("{}/.well-known/jwks", issuer.trim_end_matches('/')),
            client,
        )
    }

    /// Returns the key with `kid`, refetching once (subject to a cooldown) when it is unknown.
    pub async fn key(&self, kid: &str) -> Result<Jwk, JwksError> {
        if let Some(jwk) = self.set(false).await?.find(kid) {
            return Ok(jwk.clone());
        }
        self.set(true)
            .await?
            .find(kid)
            .cloned()
            .ok_or(JwksError::UnknownKey)
    }

    async fn set(&self, force_refresh: bool) -> Result<JwkSet, JwksError> {
        if let Some(set) = self.fresh(force_refresh).await {
            return Ok(set);
        }
        let _singleflight = self.refresh_lock.lock().await;
        if let Some(set) = self.fresh(force_refresh).await {
            return Ok(set);
        }
        let set = self.fetch().await?;
        *self.cache.write().await = Some((set.clone(), Instant::now()));
        Ok(set)
    }

    async fn fresh(&self, force_refresh: bool) -> Option<JwkSet> {
        let guard = self.cache.read().await;
        let (set, fetched_at) = guard.as_ref()?;
        let max_age = if force_refresh {
            FORCED_REFRESH_COOLDOWN
        } else {
            CACHE_TTL
        };
        (fetched_at.elapsed() < max_age).then(|| set.clone())
    }

    async fn fetch(&self) -> Result<JwkSet, JwksError> {
        let resp = self
            .client
            .get(&self.url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|_| JwksError::Unavailable)?;
        if !resp.status().is_success() {
            return Err(JwksError::Unavailable);
        }
        let body = resp.bytes().await.map_err(|_| JwksError::Unavailable)?;
        if body.len() > MAX_JWKS_BYTES {
            return Err(JwksError::Unavailable);
        }
        serde_json::from_slice(&body).map_err(|_| JwksError::Unavailable)
    }
}
