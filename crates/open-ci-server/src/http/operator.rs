//! UI-ready Operator REST API, live SSE event stream, and Prometheus metrics.

use crate::ControlPlane;
use crate::http::{ApiError, extract_bearer};
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use hmac::{Hmac, Mac};
use open_ci_core::reconcile::active_workers_by_provider;
use open_ci_core::{Capacity, ControlMode, WorkerPhase};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

type HmacSha256 = Hmac<Sha256>;
const MAX_OPERATOR_TEXT_LEN: usize = 256;

pub fn routes(state: ControlPlane) -> Router<ControlPlane> {
    Router::new()
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/pools", get(pools))
        .route("/api/v1/workers", get(workers))
        .route("/api/v1/jobs", get(jobs))
        .route("/api/v1/admissions", get(admissions))
        .route("/api/v1/backoffs", get(backoffs))
        .route("/api/v1/audit", get(audit))
        .route("/api/v1/control/mode", post(set_mode))
        .route("/api/v1/reconcile", post(trigger_reconcile))
        .route("/api/v1/events", get(events_sse))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(
            state,
            require_operator_middleware,
        ))
}

async fn require_operator_middleware(
    State(cp): State<ControlPlane>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = req.uri().path().to_owned();
    let Some(expected) = &cp.secrets.operator_token else {
        tracing::warn!(path = %path, "operator API request rejected: operator API disabled");
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "operator_api_disabled",
            "configure OPEN_CI_OPERATOR_TOKEN_FILE to enable operator API",
        ));
    };
    let provided = extract_bearer(req.headers()).ok_or_else(|| {
        tracing::warn!(path = %path, "operator API request rejected: missing bearer token");
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "operator bearer token required",
        )
    })?;

    if !constant_time_token_eq(expected.expose().as_bytes(), provided.as_bytes()) {
        tracing::warn!(path = %path, "operator API request rejected: invalid bearer token");
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid operator token",
        ));
    }
    Ok(next.run(req).await)
}

fn constant_time_token_eq(expected: &[u8], provided: &[u8]) -> bool {
    const DOMAIN_KEY: &[u8] = b"open-ci-operator-bearer-token-v1";
    let Ok(mut mac_expected) = HmacSha256::new_from_slice(DOMAIN_KEY) else {
        return false;
    };
    mac_expected.update(expected);
    let expected_tag = mac_expected.finalize().into_bytes();

    let Ok(mut mac_provided) = HmacSha256::new_from_slice(DOMAIN_KEY) else {
        return false;
    };
    mac_provided.update(provided);
    mac_provided.verify_slice(&expected_tag).is_ok()
}

async fn overview(State(cp): State<ControlPlane>) -> Result<Json<Value>, ApiError> {
    let now = cp.clock.now_unix();
    let control = cp
        .store
        .control_state()
        .await
        .map_err(crate::ServerError::Store)?;
    let all_workers = cp
        .store
        .list_workers(None)
        .await
        .map_err(crate::ServerError::Store)?;
    let held = cp
        .store
        .list_held_admissions()
        .await
        .map_err(crate::ServerError::Store)?;
    let backoffs = cp
        .store
        .list_active_backoffs(now)
        .await
        .map_err(crate::ServerError::Store)?;

    let mut used = cp.config.admission.background;
    for adm in &held {
        if let Ok(sum) = used.checked_add(adm.capacity) {
            used = sum;
        }
    }

    let mut by_phase: BTreeMap<&str, usize> = BTreeMap::new();
    for w in &all_workers {
        *by_phase.entry(w.phase.as_str()).or_insert(0) += 1;
    }

    Ok(Json(json!({
        "repository": cp.config.repository,
        "organization": cp.config.organization,
        "scope": cp.config.github_scope().display_target(),
        "admission_mode": cp.config.admission.mode,
        "timestamp": now,
        "control": {
            "mode": control.mode,
            "updated_at": control.updated_at,
            "updated_by": control.updated_by,
            "reason": control.reason,
        },
        "capacity": {
            "limit": cp.config.admission.capacity,
            "reserved": used,
            "held_admissions": held.len(),
        },
        "workers": {
            "active": all_workers.iter().filter(|w| w.phase.is_active()).count(),
            "by_phase": by_phase,
            "by_provider": active_workers_by_provider(&all_workers),
        },
        "active_backoffs": backoffs.len(),
        "pools_configured": cp.config.pools.len(),
    })))
}

async fn pools(State(cp): State<ControlPlane>) -> Result<Json<Value>, ApiError> {
    let workers = cp
        .store
        .list_workers(None)
        .await
        .map_err(crate::ServerError::Store)?;
    let mut items = Vec::new();
    for (id, pool) in &cp.config.pools {
        let active = workers
            .iter()
            .filter(|w| w.pool_id == *id && w.phase.is_active())
            .count();
        items.push(json!({
            "id": id,
            "runner_label": pool.runner_label,
            "fallback_label": pool.fallback_label,
            "architecture": pool.architecture,
            "vcpu": pool.vcpu,
            "disk_gb": pool.disk_gb,
            "max_instances": pool.max_instances,
            "active_workers": active,
            "placements": pool.placements.len(),
        }));
    }
    Ok(Json(json!({ "pools": items })))
}

#[derive(Deserialize)]
struct PoolFilter {
    pool_id: Option<String>,
}

async fn workers(
    State(cp): State<ControlPlane>,
    Query(filter): Query<PoolFilter>,
) -> Result<Json<Value>, ApiError> {
    let list = cp
        .store
        .list_workers(filter.pool_id.as_deref())
        .await
        .map_err(crate::ServerError::Store)?;
    Ok(Json(json!({ "workers": list })))
}

async fn jobs(
    State(cp): State<ControlPlane>,
    Query(filter): Query<PoolFilter>,
) -> Result<Json<Value>, ApiError> {
    let list = cp
        .store
        .list_jobs(filter.pool_id.as_deref())
        .await
        .map_err(crate::ServerError::Store)?;
    Ok(Json(json!({ "jobs": list })))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    100
}

async fn admissions(
    State(cp): State<ControlPlane>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    let list = cp
        .store
        .list_admissions(q.limit.min(500))
        .await
        .map_err(crate::ServerError::Store)?;
    Ok(Json(json!({ "admissions": list })))
}

async fn backoffs(State(cp): State<ControlPlane>) -> Result<Json<Value>, ApiError> {
    let list = cp
        .store
        .list_active_backoffs(cp.clock.now_unix())
        .await
        .map_err(crate::ServerError::Store)?;
    Ok(Json(json!({ "backoffs": list })))
}

async fn audit(
    State(cp): State<ControlPlane>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    let list = cp
        .store
        .list_audit(q.limit.min(500))
        .await
        .map_err(crate::ServerError::Store)?;
    Ok(Json(json!({ "entries": list })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetModePayload {
    mode: ControlMode,
    #[serde(default = "default_actor")]
    actor: String,
    #[serde(default = "default_reason")]
    reason: String,
}

fn default_actor() -> String {
    "operator".to_owned()
}
fn default_reason() -> String {
    "operator_api".to_owned()
}

async fn set_mode(
    State(cp): State<ControlPlane>,
    Json(payload): Json<SetModePayload>,
) -> Result<Json<Value>, ApiError> {
    if payload.actor.len() > MAX_OPERATOR_TEXT_LEN || payload.reason.len() > MAX_OPERATOR_TEXT_LEN {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "actor and reason must be at most 256 bytes",
        ));
    }
    let state = cp
        .set_mode(payload.mode, &payload.actor, &payload.reason)
        .await?;
    Ok(Json(json!({
        "mode": state.mode,
        "updated_at": state.updated_at,
        "updated_by": state.updated_by,
        "reason": state.reason,
    })))
}

async fn trigger_reconcile(
    State(cp): State<ControlPlane>,
) -> Result<Json<crate::reconciler::ReconcileSummary>, ApiError> {
    let summary = cp.reconcile_once().await?;
    let detail = serde_json::to_value(&summary).unwrap_or_else(|_| json!({}));
    let _ = cp
        .store
        .append_audit(
            cp.clock.now_unix(),
            "operator_api",
            "reconcile",
            "control_plane",
            detail,
        )
        .await;
    Ok(Json(summary))
}

async fn events_sse(
    State(cp): State<ControlPlane>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = cp.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(event) => serde_json::to_string(&event)
            .ok()
            .map(|data| Ok(Event::default().event("control_event").data(data))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn metrics(State(cp): State<ControlPlane>) -> Result<String, ApiError> {
    let now = cp.clock.now_unix();
    let workers = cp
        .store
        .list_workers(None)
        .await
        .map_err(crate::ServerError::Store)?;
    let held = cp
        .store
        .list_held_admissions()
        .await
        .map_err(crate::ServerError::Store)?;
    let backoffs = cp
        .store
        .list_active_backoffs(now)
        .await
        .map_err(crate::ServerError::Store)?;

    let mut reserved = cp.config.admission.background;
    for h in &held {
        if let Ok(sum) = reserved.checked_add(h.capacity) {
            reserved = sum;
        }
    }

    let mut out = String::new();
    out.push_str("# HELP open_ci_workers_total Workers by phase\n");
    out.push_str("# TYPE open_ci_workers_total gauge\n");
    for phase in [
        WorkerPhase::Planned,
        WorkerPhase::Creating,
        WorkerPhase::Live,
        WorkerPhase::Retiring,
        WorkerPhase::Gone,
    ] {
        let count = workers.iter().filter(|w| w.phase == phase).count();
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "open_ci_workers_total{{phase=\"{}\"}} {count}\n",
                phase.as_str()
            ),
        );
    }
    append_capacity_metric(&mut out, "open_ci_capacity_reserved", reserved);
    append_capacity_metric(
        &mut out,
        "open_ci_capacity_limit",
        cp.config.admission.capacity,
    );
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "open_ci_held_admissions {}\nopen_ci_active_backoffs {}\n",
            held.len(),
            backoffs.len()
        ),
    );
    Ok(out)
}

fn append_capacity_metric(out: &mut String, prefix: &str, cap: Capacity) {
    let _ = std::fmt::Write::write_fmt(
        out,
        format_args!(
            "{prefix}_vcpu {}\n{prefix}_instances {}\n{prefix}_disk_gb {}\n",
            cap.vcpu, cap.instances, cap.disk_gb
        ),
    );
}
