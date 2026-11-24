//! Scoped one-time bootstrap capabilities, usage token verification, and
//! worker agent cloud-init payload rendering (zero Bash/Python string hacks).

use crate::{ControlPlane, ServerError};
use hmac::{Hmac, Mac};
use open_ci_core::{SecretString, WorkerPhase, WorkerUsage};
use serde::Serialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Computes the worker-scoped one-time bootstrap bearer token.
pub fn mint_bootstrap_token(
    secret: &SecretString,
    worker_name: &str,
) -> Result<String, ServerError> {
    hmac_hex(secret, format!("bootstrap:{worker_name}").as_bytes())
}

/// Computes the worker-scoped usage reporting bearer token.
pub fn mint_usage_token(secret: &SecretString, worker_name: &str) -> Result<String, ServerError> {
    hmac_hex(secret, format!("usage:{worker_name}").as_bytes())
}

/// Constant-time verification of a worker-scoped HMAC token.
pub fn verify_token(
    secret: &SecretString,
    purpose: &str,
    worker_name: &str,
    provided_hex: &str,
) -> Result<(), ServerError> {
    let provided = hex::decode(provided_hex).map_err(|_| ServerError::Unauthorized)?;
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| ServerError::Unauthorized)?;
    mac.update(format!("{purpose}:{worker_name}").as_bytes());
    mac.verify_slice(&provided)
        .map_err(|_| ServerError::Unauthorized)
}

fn hmac_hex(secret: &SecretString, msg: &[u8]) -> Result<String, ServerError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| ServerError::InvalidConfig("invalid hmac key".to_owned()))?;
    mac.update(msg);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Structured cloud-init payload consumed by `open-ci agent bootstrap` baked into the Packer image.
#[derive(Serialize)]
struct AgentBootstrapEnvelope<'a> {
    public_url: &'a str,
    worker_name: &'a str,
    bootstrap_token: String,
    usage_token: String,
    lifetime_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_url: Option<&'a str>,
}

/// Renders minimal, safe cloud-init user-data that hands JSON via stdin to `open-ci agent bootstrap`.
pub fn render_user_data(
    secret: &SecretString,
    public_url: &str,
    worker_name: &str,
    lifetime_seconds: u64,
    cache_url: Option<&str>,
) -> Result<String, ServerError> {
    let envelope = AgentBootstrapEnvelope {
        public_url,
        worker_name,
        bootstrap_token: mint_bootstrap_token(secret, worker_name)?,
        usage_token: mint_usage_token(secret, worker_name)?,
        lifetime_seconds,
        cache_url,
    };
    let json = serde_json::to_string(&envelope)
        .map_err(|_| ServerError::InvalidConfig("failed serializing agent envelope".to_owned()))?;
    Ok(format!(
        "#!/bin/sh\nset -eu\nexec /usr/local/bin/open-ci agent bootstrap <<'OPEN_CI_EOF'\n{json}\nOPEN_CI_EOF\n"
    ))
}

impl ControlPlane {
    /// Atomically burns a worker's one-time bootstrap capability and returns a GitHub JIT runner config.
    pub async fn handle_bootstrap(
        &self,
        worker_name: &str,
        bearer_token: &str,
    ) -> Result<SecretString, ServerError> {
        verify_token(
            &self.secrets.bootstrap_secret,
            "bootstrap",
            worker_name,
            bearer_token,
        )?;

        let state = self.store.control_state().await?;
        if !state.mode.allows_new_workers() {
            return Err(ServerError::BootstrapUnavailable);
        }

        let now = self.clock.now_unix();
        let Some(existing) = self.store.get_worker(worker_name).await? else {
            return Err(ServerError::BootstrapUnavailable);
        };
        let Some(pool) = self.config.pools.get(&existing.pool_id) else {
            return Err(ServerError::BootstrapUnavailable);
        };

        if existing.phase != WorkerPhase::Live
            || existing.bootstrap_used
            || now >= existing.created_at.saturating_add(pool.idle_seconds)
        {
            return Err(ServerError::BootstrapUnavailable);
        }

        // Burn capability atomically BEFORE calling GitHub's non-idempotent JIT endpoint.
        let mut worker = self
            .store
            .consume_bootstrap(worker_name, now)
            .await
            .map_err(|_| ServerError::BootstrapUnavailable)?;

        let label = match &worker.admission_id {
            Some(res_id) => match self.store.get_admission(res_id).await? {
                Some(adm) => adm
                    .runners
                    .get(&worker.pool_id)
                    .cloned()
                    .unwrap_or_else(|| format!("{}-{res_id}", pool.runner_label)),
                None => format!("{}-{res_id}", pool.runner_label),
            },
            None => pool.runner_label.clone(),
        };

        match self
            .github
            .generate_jit_config(&worker.name, pool.architecture, &label)
            .await
        {
            Ok(jit) => Ok(jit),
            Err(err) => {
                worker.phase = WorkerPhase::Retiring;
                worker.retire_reason = Some("bootstrap_failed".to_owned());
                worker.updated_at = self.clock.now_unix();
                let _ = self.store.upsert_worker(&worker).await;
                Err(ServerError::GitHub(err))
            }
        }
    }

    /// Validates and persists a worker's bounded NIC usage telemetry sample.
    pub async fn handle_usage(
        &self,
        worker_name: &str,
        bearer_token: &str,
        usage: WorkerUsage,
    ) -> Result<(), ServerError> {
        verify_token(
            &self.secrets.bootstrap_secret,
            "usage",
            worker_name,
            bearer_token,
        )?;

        let now = self.clock.now_unix();
        let Some(worker) = self.store.get_worker(worker_name).await? else {
            return Err(ServerError::Unauthorized);
        };
        let Some(pool) = self.config.pools.get(&worker.pool_id) else {
            return Err(ServerError::Unauthorized);
        };

        if !worker.bootstrap_used
            || now
                >= worker
                    .created_at
                    .saturating_add(pool.lifetime_seconds)
                    .saturating_add(120)
        {
            return Err(ServerError::BadRequest("usage window closed".to_owned()));
        }
        if usage.rx_bytes == 0
            || usage.tx_bytes == 0
            || usage.rx_bytes >= (1_u64 << 60)
            || usage.tx_bytes >= (1_u64 << 60)
        {
            return Err(ServerError::BadRequest("usage out of bounds".to_owned()));
        }

        self.store.record_usage(worker_name, usage, now).await?;
        Ok(())
    }
}
