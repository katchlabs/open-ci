//! Public machine ingress endpoints (health, GitHub webhook, OIDC admission, worker bootstrap/usage).

use crate::ControlPlane;
use crate::http::{ApiError, extract_bearer};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use open_ci_core::{AdmissionRequest, AdmissionStatus, ControlEvent, Job, JobStatus, WorkerUsage};
use open_ci_github::{WebhookError, WorkflowJobEvent, verify_and_parse_webhook};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn routes() -> Router<ControlPlane> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/ingress/webhook", post(webhook))
        .route("/v1/ingress/admit", post(admit))
        .route("/v1/workers/{name}/bootstrap", post(bootstrap))
        .route(
            "/v1/workers/{name}/usage",
            post(usage).layer(DefaultBodyLimit::max(512)),
        )
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(cp): State<ControlPlane>) -> Result<Json<Value>, ApiError> {
    let _ = cp.store.control_state().await.map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_unavailable",
            "database check failed",
        )
    })?;
    Ok(Json(json!({ "status": "ready" })))
}

async fn webhook(
    State(cp): State<ControlPlane>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let scope = cp.config.github_scope();
    let parsed =
        match verify_and_parse_webhook(&cp.secrets.webhook_secret, sig, event, &body, &scope) {
            Ok(ev) => ev,
            Err(WebhookError::InvalidSignature) => {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid_signature",
                    "webhook signature invalid",
                ));
            }
            Err(WebhookError::IgnoredEvent(_) | WebhookError::UnsupportedAction(_)) => {
                return Ok(StatusCode::NO_CONTENT);
            }
            Err(WebhookError::RepositoryMismatch(_)) => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "repository_not_allowed",
                    "repository is not allowlisted",
                ));
            }
            Err(WebhookError::MalformedPayload) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "malformed_payload",
                    "invalid workflow_job payload",
                ));
            }
        };

    if cp.config.admission.mode.allows_webhook_admission() {
        handle_webhook_auto_admission(&cp, &parsed).await
    } else {
        handle_oidc_mode_webhook(&cp, &parsed).await
    }
}

async fn record_webhook_job(
    cp: &ControlPlane,
    parsed: &WorkflowJobEvent,
    pool_id: &str,
    admission_id: Option<&str>,
    now: u64,
) -> Result<(), ApiError> {
    if matches!(parsed.action.as_str(), "in_progress" | "completed") {
        let status = JobStatus::parse(&parsed.action);
        let job = Job {
            id: parsed.job_id,
            pool_id: pool_id.to_owned(),
            repository: Some(parsed.repository.clone()),
            admission_id: admission_id.map(str::to_owned),
            run_id: Some(parsed.run_id),
            status,
            conclusion: parsed.conclusion.clone(),
            runner_name: parsed.runner_name.clone(),
            runner_id: parsed.runner_id,
            checked_at: Some(now),
            updated_at: now,
        };
        cp.store
            .upsert_job(&job)
            .await
            .map_err(crate::ServerError::Store)?;
        cp.emit(ControlEvent::JobChanged {
            job_id: job.id,
            pool_id: pool_id.to_owned(),
            status,
            timestamp: now,
        });
    } else {
        cp.store
            .observe_job_hint(
                parsed.job_id,
                pool_id,
                Some(&parsed.repository),
                admission_id,
                Some(parsed.run_id),
                now,
            )
            .await
            .map_err(crate::ServerError::Store)?;
    }
    Ok(())
}

async fn handle_oidc_mode_webhook(
    cp: &ControlPlane,
    parsed: &WorkflowJobEvent,
) -> Result<StatusCode, ApiError> {
    let now = cp.clock.now_unix();
    let held = cp
        .store
        .list_held_admissions()
        .await
        .map_err(crate::ServerError::Store)?;
    let matching_admission = held.iter().find(|a| {
        a.repository == parsed.repository
            && a.run_id == parsed.run_id
            && a.run_attempt == parsed.run_attempt
    });

    let mut matched = false;
    for (pool_id, pool) in &cp.config.pools {
        let scoped = matching_admission.map(|adm| {
            adm.runners
                .get(pool_id)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}", pool.runner_label, adm.id))
        });
        if pool.accepts_labels(&parsed.labels, scoped.as_deref())
            || pool.accepts_labels(&parsed.labels, None)
        {
            record_webhook_job(
                cp,
                parsed,
                pool_id,
                matching_admission.map(|a| a.id.as_str()),
                now,
            )
            .await?;
            matched = true;
        }
    }

    if matched {
        cp.request_reconcile();
        Ok(StatusCode::ACCEPTED)
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

async fn handle_webhook_auto_admission(
    cp: &ControlPlane,
    parsed: &WorkflowJobEvent,
) -> Result<StatusCode, ApiError> {
    let now = cp.clock.now_unix();
    if let Some(existing) = cp
        .store
        .get_admission_by_run(&parsed.repository, parsed.run_id, parsed.run_attempt)
        .await
        .map_err(crate::ServerError::Store)?
    {
        if existing.status != AdmissionStatus::Held {
            return Ok(StatusCode::NO_CONTENT);
        }
        let mut matched = false;
        for (pool_id, pool) in &cp.config.pools {
            let Some(expected_label) = existing.runners.get(pool_id) else {
                continue;
            };
            if pool.accepts_labels(&parsed.labels, Some(expected_label)) {
                record_webhook_job(cp, parsed, pool_id, Some(&existing.id), now).await?;
                matched = true;
            }
        }
        if matched {
            cp.request_reconcile();
            return Ok(StatusCode::ACCEPTED);
        }
        return Ok(StatusCode::NO_CONTENT);
    }

    let matched_pools: BTreeSet<String> = cp
        .config
        .pools
        .iter()
        .filter(|(_, pool)| pool.accepts_labels(&parsed.labels, None))
        .map(|(pool_id, _)| pool_id.clone())
        .collect();
    if matched_pools.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }

    if matches!(parsed.action.as_str(), "in_progress" | "completed") {
        for pool_id in &matched_pools {
            record_webhook_job(cp, parsed, pool_id, None, now).await?;
        }
        cp.request_reconcile();
        return Ok(StatusCode::ACCEPTED);
    }

    if parsed.action != "queued" {
        return Ok(StatusCode::NO_CONTENT);
    }

    let req = AdmissionRequest {
        repository: parsed.repository.clone(),
        run_id: parsed.run_id,
        run_attempt: parsed.run_attempt,
        head_sha: parsed.head_sha.clone(),
        pools: matched_pools.clone(),
    };
    let decision = cp.admit_webhook(&req).await?;
    if !decision.admitted {
        return Ok(StatusCode::NO_CONTENT);
    }

    for pool_id in &matched_pools {
        record_webhook_job(cp, parsed, pool_id, Some(&decision.reservation), now).await?;
    }

    cp.request_reconcile();
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmitPayload {
    pools: BTreeSet<String>,
}

async fn admit(
    State(cp): State<ControlPlane>,
    headers: HeaderMap,
    Json(payload): Json<AdmitPayload>,
) -> Result<Json<open_ci_core::AdmissionDecision>, ApiError> {
    if !cp.config.admission.mode.allows_oidc_admission() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "oidc_admission_disabled",
            "OIDC admission is disabled when admission.mode is `webhook`",
        ));
    }
    let Some(oidc) = &cp.oidc else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "oidc_admission_disabled",
            "OIDC verifier is not configured",
        ));
    };

    let token = extract_bearer(&headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing_token",
            "OIDC bearer token required",
        )
    })?;

    let claims = oidc.verify(token).await.map_err(|_| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_oidc_token",
            "OIDC verification failed",
        )
    })?;

    let req = AdmissionRequest {
        repository: claims.repository,
        run_id: claims.run_id,
        run_attempt: claims.run_attempt,
        head_sha: claims.sha,
        pools: payload.pools,
    };

    let decision = cp.admit(&req).await?;
    if decision.admitted {
        cp.request_reconcile();
    }
    Ok(Json(decision))
}

async fn bootstrap(
    State(cp): State<ControlPlane>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, [(HeaderName, &'static str); 1], String), ApiError> {
    let token = extract_bearer(&headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing_token",
            "bootstrap bearer token required",
        )
    })?;
    let jit = cp.handle_bootstrap(&name, token).await?;
    Ok((
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        jit.expose().to_owned(),
    ))
}

async fn usage(
    State(cp): State<ControlPlane>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(sample): Json<WorkerUsage>,
) -> Result<StatusCode, ApiError> {
    let token = extract_bearer(&headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing_token",
            "usage bearer token required",
        )
    })?;
    cp.handle_usage(&name, token, sample).await?;
    Ok(StatusCode::ACCEPTED)
}
