//! Pure capacity accounting and admission evaluation.

use crate::config::OpenCiConfig;
use crate::error::DomainError;
use crate::models::{
    AdmissionDecision, AdmissionReason, AdmissionRecord, AdmissionRequest, AdmissionStatus,
    Capacity, ControlMode,
};
use std::collections::BTreeMap;

/// Snapshot of durable ledger state required to evaluate an admission request.
#[derive(Clone, Debug, Default)]
pub struct AdmissionContext {
    pub mode: ControlMode,
    pub held_admissions: Vec<AdmissionRecord>,
    pub total_admitted_count: u32,
    pub utc_day_admitted_count: u32,
    pub providers_healthy: bool,
}

/// Evaluates whether a workflow run should be admitted to `open-ci` capacity or
/// routed to configured fallback runner labels (with per-reservation scoped labels).
///
/// Pure and deterministic: callers persist the resulting decision inside a
/// serialized transaction to guarantee mutual exclusion across concurrent runs.
pub fn evaluate_admission(
    config: &OpenCiConfig,
    ctx: &AdmissionContext,
    request: &AdmissionRequest,
    reservation_id: &str,
    now_unix: u64,
) -> Result<AdmissionDecision, DomainError> {
    evaluate_admission_inner(config, ctx, request, reservation_id, now_unix, true)
}

/// Evaluates direct webhook admission using unscoped pool runner labels so existing
/// workflows work without `actions/route` label rewriting.
pub fn evaluate_webhook_admission(
    config: &OpenCiConfig,
    ctx: &AdmissionContext,
    request: &AdmissionRequest,
    reservation_id: &str,
    now_unix: u64,
) -> Result<AdmissionDecision, DomainError> {
    evaluate_admission_inner(config, ctx, request, reservation_id, now_unix, false)
}

fn evaluate_admission_inner(
    config: &OpenCiConfig,
    ctx: &AdmissionContext,
    request: &AdmissionRequest,
    reservation_id: &str,
    now_unix: u64,
    scoped_labels: bool,
) -> Result<AdmissionDecision, DomainError> {
    if !config.github_scope().allows_repository(&request.repository) {
        return Err(DomainError::InvalidRequest(
            "repository is not allowed by configured GitHub scope".to_owned(),
        ));
    }
    if request.run_id == 0 || request.run_attempt == 0 {
        return Err(DomainError::InvalidRequest(
            "run_id and run_attempt must be positive".to_owned(),
        ));
    }

    let mut fallback_runners = BTreeMap::new();
    let mut requested_footprint = Capacity::default();

    for pool_id in &request.pools {
        let pool = config
            .pools
            .get(pool_id)
            .ok_or_else(|| DomainError::UnknownPool(pool_id.clone()))?;
        fallback_runners.insert(pool_id.clone(), pool.fallback_label.clone());
        let fp = pool
            .capacity_footprint()
            .map_err(|_| DomainError::CapacityOverflow)?;
        requested_footprint = requested_footprint.checked_add(fp)?;
    }

    let fallback = |reason: AdmissionReason| AdmissionDecision {
        admitted: false,
        target: "fallback".to_owned(),
        reason,
        reservation: String::new(),
        runners: fallback_runners.clone(),
    };

    if let Some(reason) = check_eligibility(config, ctx, request, now_unix) {
        return Ok(fallback(reason));
    }

    if !has_sufficient_capacity(config, ctx, requested_footprint)? {
        return Ok(fallback(AdmissionReason::Capacity));
    }

    let mut runners = BTreeMap::new();
    for pool_id in &request.pools {
        let pool = config
            .pools
            .get(pool_id)
            .ok_or_else(|| DomainError::UnknownPool(pool_id.clone()))?;
        let label = if scoped_labels {
            format!("{}-{reservation_id}", pool.runner_label)
        } else {
            pool.runner_label.clone()
        };
        runners.insert(pool_id.clone(), label);
    }

    Ok(AdmissionDecision {
        admitted: true,
        target: "open-ci".to_owned(),
        reason: AdmissionReason::Admitted,
        reservation: reservation_id.to_owned(),
        runners,
    })
}

fn check_eligibility(
    config: &OpenCiConfig,
    ctx: &AdmissionContext,
    request: &AdmissionRequest,
    now_unix: u64,
) -> Option<AdmissionReason> {
    if request.pools.is_empty() {
        return Some(AdmissionReason::NoWork);
    }
    if request.run_attempt > 1 && config.admission.fallback_on_rerun {
        return Some(AdmissionReason::Rerun);
    }
    if !ctx.mode.allows_new_admissions() {
        return Some(AdmissionReason::ControlMode);
    }
    if !ctx.providers_healthy {
        return Some(AdmissionReason::Unavailable);
    }
    let guardrails = &config.admission.guardrails;
    if guardrails
        .admit_until_unix
        .is_some_and(|deadline| now_unix >= deadline)
    {
        return Some(AdmissionReason::Deadline);
    }
    if guardrails
        .max_admitted_runs
        .is_some_and(|cap| ctx.total_admitted_count >= cap)
    {
        return Some(AdmissionReason::LifetimeLimit);
    }
    if guardrails
        .max_admitted_runs_per_utc_day
        .is_some_and(|cap| ctx.utc_day_admitted_count >= cap)
    {
        return Some(AdmissionReason::DailyLimit);
    }
    None
}

fn has_sufficient_capacity(
    config: &OpenCiConfig,
    ctx: &AdmissionContext,
    requested_footprint: Capacity,
) -> Result<bool, DomainError> {
    let active_held: Vec<&AdmissionRecord> = ctx
        .held_admissions
        .iter()
        .filter(|r| r.status == AdmissionStatus::Held)
        .collect();

    if config
        .admission
        .guardrails
        .max_concurrent_runs
        .is_some_and(|cap| active_held.len() >= cap as usize)
    {
        return Ok(false);
    }

    let mut used = config.admission.background;
    for held in active_held {
        used = used.checked_add(held.capacity)?;
    }
    let projected = used.checked_add(requested_footprint)?;
    Ok(projected.fits_within(config.admission.capacity))
}

/// Computes the exact capacity footprint for a set of requested pools.
pub fn requested_capacity(
    config: &OpenCiConfig,
    pools: &std::collections::BTreeSet<String>,
) -> Result<Capacity, DomainError> {
    let mut total = Capacity::default();
    for pool_id in pools {
        let pool = config
            .pools
            .get(pool_id)
            .ok_or_else(|| DomainError::UnknownPool(pool_id.clone()))?;
        let fp = pool
            .capacity_footprint()
            .map_err(|_| DomainError::CapacityOverflow)?;
        total = total.checked_add(fp)?;
    }
    Ok(total)
}
