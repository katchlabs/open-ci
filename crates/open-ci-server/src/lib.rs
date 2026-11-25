//! Unified, non-blocking `open-ci` control plane, reconciler, and HTTP/SSE server.

pub mod bootstrap;
pub mod http;
pub mod reconciler;

use open_ci_core::admission::{
    AdmissionContext, evaluate_admission, evaluate_webhook_admission, requested_capacity,
};
use open_ci_core::{
    AdmissionDecision, AdmissionRecord, AdmissionRequest, AdmissionStatus, Clock, ControlEvent,
    ControlMode, OpenCiConfig, SecretString,
};
use open_ci_github::{GitHubClient, OidcVerifier};
use open_ci_provider::ProviderRegistry;
use open_ci_store::{Store, StoreError};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

/// Runtime secrets held exclusively in redacted [`SecretString`] wrappers.
#[derive(Clone, Debug)]
pub struct ServerSecrets {
    pub webhook_secret: SecretString,
    pub bootstrap_secret: SecretString,
    pub operator_token: Option<SecretString>,
}

impl ServerSecrets {
    pub fn validate(&self) -> Result<(), ServerError> {
        if self.webhook_secret.len() < 32 || self.bootstrap_secret.len() < 32 {
            return Err(ServerError::InvalidConfig(
                "webhook_secret and bootstrap_secret must be at least 32 bytes".to_owned(),
            ));
        }
        if let Some(tok) = &self.operator_token
            && tok.len() < 24
        {
            return Err(ServerError::InvalidConfig(
                "operator_token must be at least 24 bytes when set".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("invalid server configuration: {0}")]
    InvalidConfig(String),

    #[error("storage error: {0}")]
    Store(#[from] StoreError),

    #[error("domain error: {0}")]
    Domain(#[from] open_ci_core::DomainError),

    #[error("github error: {0}")]
    GitHub(#[from] open_ci_github::GitHubError),

    #[error("provider error: {0}")]
    Provider(#[from] open_ci_provider::ProviderError),

    #[error("unauthorized")]
    Unauthorized,

    #[error("bootstrap unavailable")]
    BootstrapUnavailable,

    #[error("invalid request: {0}")]
    BadRequest(String),
}

/// Shared, lock-free-for-reads control-plane handle.
///
/// Notice there is NO global `Mutex<Controller>` held across network calls:
/// HTTP handlers and reconciliation ticks operate concurrently against the
/// non-blocking `Store`, using only a fine-grained `admission_tx` mutex to
/// serialize admission capacity checks.
#[derive(Clone, Debug)]
pub struct ControlPlane {
    pub config: Arc<OpenCiConfig>,
    pub store: Arc<dyn Store>,
    pub providers: Arc<ProviderRegistry>,
    pub github: Arc<GitHubClient>,
    pub oidc: Option<Arc<OidcVerifier>>,
    pub clock: Arc<dyn Clock>,
    pub secrets: Arc<ServerSecrets>,
    pub events: broadcast::Sender<ControlEvent>,
    admission_tx: Arc<Mutex<()>>,
}

impl ControlPlane {
    pub fn new(
        config: OpenCiConfig,
        store: Arc<dyn Store>,
        providers: ProviderRegistry,
        github: GitHubClient,
        oidc: Option<OidcVerifier>,
        clock: Arc<dyn Clock>,
        secrets: ServerSecrets,
    ) -> Result<Self, ServerError> {
        secrets.validate()?;
        if config.admission.mode.allows_oidc_admission() && oidc.is_none() {
            return Err(ServerError::InvalidConfig(
                "OIDC verifier is required when admission.mode is `oidc` or `hybrid`".to_owned(),
            ));
        }
        let (events, _) = broadcast::channel(512);
        Ok(Self {
            config: Arc::new(config),
            store,
            providers: Arc::new(providers),
            github: Arc::new(github),
            oidc: oidc.map(Arc::new),
            clock,
            secrets: Arc::new(secrets),
            events,
            admission_tx: Arc::new(Mutex::new(())),
        })
    }

    pub fn emit(&self, event: ControlEvent) {
        let _ = self.events.send(event);
    }

    /// Evaluates and durably persists an idempotent OIDC run admission decision (scoped runner labels).
    pub async fn admit(
        &self,
        request: &AdmissionRequest,
    ) -> Result<AdmissionDecision, ServerError> {
        self.admit_with_mode(request, true).await
    }

    /// Evaluates and durably persists an idempotent direct webhook admission decision
    /// using unscoped pool runner labels so workflows require zero `actions/route` changes.
    pub async fn admit_webhook(
        &self,
        request: &AdmissionRequest,
    ) -> Result<AdmissionDecision, ServerError> {
        self.admit_with_mode(request, false).await
    }

    async fn admit_with_mode(
        &self,
        request: &AdmissionRequest,
        scoped_labels: bool,
    ) -> Result<AdmissionDecision, ServerError> {
        // Provider health check runs outside the admission serialization lock.
        let providers_healthy = self.check_providers_healthy(&request.pools).await;

        let _guard = self.admission_tx.lock().await;
        if let Some(existing) = self
            .store
            .get_admission_by_run(&request.repository, request.run_id, request.run_attempt)
            .await?
        {
            return Ok(AdmissionDecision {
                admitted: existing.status != AdmissionStatus::Fallback,
                target: existing.target,
                reason: existing.reason,
                reservation: if existing.status == AdmissionStatus::Fallback {
                    String::new()
                } else {
                    existing.id
                },
                runners: existing.runners,
            });
        }

        let now = self.clock.now_unix();
        let utc_day_start = now - (now % 86_400);
        let control_state = self.store.control_state().await?;
        let held_admissions = self.store.list_held_admissions().await?;
        let (total_admitted_count, utc_day_admitted_count) =
            self.store.count_admitted(utc_day_start).await?;

        let mut pool_launched_counts = std::collections::BTreeMap::new();
        for pool_id in &request.pools {
            if self
                .config
                .pools
                .get(pool_id)
                .is_some_and(|pool| pool.max_total_instances.is_some())
            {
                let launched = self.store.count_pool_workers(pool_id).await?;
                pool_launched_counts.insert(pool_id.clone(), launched);
            }
        }

        let ctx = AdmissionContext {
            mode: control_state.mode,
            held_admissions,
            total_admitted_count,
            utc_day_admitted_count,
            providers_healthy,
            pool_launched_counts,
        };

        let reservation_id = format!("open-ci-{}", Uuid::new_v4().simple());
        let decision = if scoped_labels {
            evaluate_admission(&self.config, &ctx, request, &reservation_id, now)?
        } else {
            evaluate_webhook_admission(&self.config, &ctx, request, &reservation_id, now)?
        };
        let capacity = if decision.admitted {
            requested_capacity(&self.config, &request.pools)?
        } else {
            open_ci_core::Capacity::default()
        };
        let status = if decision.admitted {
            AdmissionStatus::Held
        } else {
            AdmissionStatus::Fallback
        };

        let record = AdmissionRecord {
            id: reservation_id,
            repository: request.repository.clone(),
            run_id: request.run_id,
            run_attempt: request.run_attempt,
            head_sha: request.head_sha.clone(),
            pools: request.pools.clone(),
            target: decision.target.clone(),
            reason: decision.reason,
            status,
            capacity,
            runners: decision.runners.clone(),
            created_at: now,
            released_at: (!decision.admitted).then_some(now),
        };
        self.store.save_admission(&record).await?;
        self.emit(ControlEvent::AdmissionChanged {
            reservation: decision.reservation.clone(),
            run_id: request.run_id,
            status,
            reason: decision.reason,
            timestamp: now,
        });
        Ok(decision)
    }

    async fn check_providers_healthy(&self, pools: &std::collections::BTreeSet<String>) -> bool {
        for pool_id in pools {
            let Some(pool) = self.config.pools.get(pool_id) else {
                return false;
            };
            let mut any_healthy = false;
            for placement in &pool.placements {
                if let Ok(provider) = self.providers.get(placement.provider)
                    && provider.health_check(placement).await.is_ok()
                {
                    any_healthy = true;
                    break;
                }
            }
            if !any_healthy {
                return false;
            }
        }
        true
    }

    /// Updates durable control mode (`active`, `fallback_only`, `draining`, `paused`) and emits an SSE event.
    pub async fn set_mode(
        &self,
        mode: ControlMode,
        actor: &str,
        reason: &str,
    ) -> Result<open_ci_store::ControlState, ServerError> {
        let now = self.clock.now_unix();
        let state = self
            .store
            .set_control_mode(mode, actor.to_owned(), reason.to_owned(), now)
            .await?;
        self.emit(ControlEvent::ControlModeChanged {
            mode,
            actor: actor.to_owned(),
            reason: reason.to_owned(),
            timestamp: now,
        });
        Ok(state)
    }
}
