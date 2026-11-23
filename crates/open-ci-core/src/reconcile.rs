//! Pure worker lifecycle state machine, retirement evaluation, and placement scheduling.

use crate::config::{Placement, PoolConfig};
use crate::error::DomainError;
use crate::models::{
    ControlMode, Job, JobStatus, PlacementBackoff, ProviderKind, Worker, WorkerPhase,
};
use std::collections::BTreeMap;

/// Reason an active worker should transition to [`WorkerPhase::Retiring`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireReason {
    LifetimeExpired,
    IdleExpired,
    DrainingIdle,
    JobCompleted,
    InstanceTerminated,
    BootstrapFailed,
    CreateFailed,
}

impl RetireReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LifetimeExpired => "lifetime_expired",
            Self::IdleExpired => "idle_expired",
            Self::DrainingIdle => "draining_idle",
            Self::JobCompleted => "job_completed",
            Self::InstanceTerminated => "instance_terminated",
            Self::BootstrapFailed => "bootstrap_failed",
            Self::CreateFailed => "create_failed",
        }
    }
}

/// Transitions a worker phase, enforcing monotonic lifecycle progression.
pub fn transition_phase(
    worker: &mut Worker,
    next: WorkerPhase,
    now: u64,
) -> Result<(), DomainError> {
    if !worker.phase.can_transition_to(next) {
        return Err(DomainError::InvalidPhaseTransition {
            from: worker.phase,
            to: next,
        });
    }
    worker.phase = next;
    worker.updated_at = now;
    Ok(())
}

/// Evaluates whether a live/creating worker must retire based on authoritative state.
pub fn evaluate_retirement(
    pool: &PoolConfig,
    worker: &Worker,
    assigned_job: Option<&Job>,
    mode: ControlMode,
    now_unix: u64,
) -> Option<RetireReason> {
    if matches!(worker.phase, WorkerPhase::Retiring | WorkerPhase::Gone) {
        return None;
    }

    if now_unix >= worker.created_at.saturating_add(pool.lifetime_seconds) {
        return Some(RetireReason::LifetimeExpired);
    }

    if let Some(job) = assigned_job {
        if job.status == JobStatus::Completed {
            return Some(RetireReason::JobCompleted);
        }
        // Busy worker with an in-progress job retains its lifetime TTL even during drain.
        return None;
    }

    if mode == ControlMode::Draining && !worker.bootstrap_used {
        return Some(RetireReason::DrainingIdle);
    }

    if !worker.bootstrap_used && now_unix >= worker.created_at.saturating_add(pool.idle_seconds) {
        return Some(RetireReason::IdleExpired);
    }

    None
}

/// Computes how many new workers should be created for a pool on the current tick.
pub fn plan_scale_up(
    pool: &PoolConfig,
    mode: ControlMode,
    active_workers: usize,
    total_launched: usize,
    queued_jobs: usize,
    unassigned_workers: usize,
) -> usize {
    if !mode.allows_new_workers() {
        return 0;
    }

    let idle_deficit = pool.min_idle_instances.saturating_sub(unassigned_workers);
    let queue_deficit = queued_jobs.saturating_sub(unassigned_workers);
    let needed = idle_deficit.max(queue_deficit);
    if needed == 0 {
        return 0;
    }

    let remaining_concurrent = pool.max_instances.saturating_sub(active_workers);
    let remaining_lifetime = pool
        .max_total_instances
        .map_or(usize::MAX, |limit| limit.saturating_sub(total_launched));

    needed
        .min(remaining_concurrent)
        .min(remaining_lifetime)
        .min(pool.creates_per_tick)
}

/// Returns `true` if a placement is currently in backoff due to quota or stockout.
pub fn is_placement_backed_off(
    placement: &Placement,
    backoffs: &[PlacementBackoff],
    now_unix: u64,
) -> bool {
    let family = placement.machine_family();
    backoffs.iter().any(|b| {
        b.provider == placement.provider
            && b.region == placement.region
            && b.family == family
            && b.retry_after > now_unix
    })
}

/// Selects the next ready placement index according to pool weights, cheapest-first,
/// or round-robin cursor, advancing the cursor when a placement is selected.
pub fn select_placement(
    pool: &PoolConfig,
    backoffs: &[PlacementBackoff],
    cursor: &mut u64,
    now_unix: u64,
) -> Option<usize> {
    if pool.placements.is_empty() {
        return None;
    }

    let ready_indices: Vec<usize> = pool
        .placements
        .iter()
        .enumerate()
        .filter(|(_, p)| !is_placement_backed_off(p, backoffs, now_unix))
        .map(|(idx, _)| idx)
        .collect();

    if ready_indices.is_empty() {
        return None;
    }

    if !pool.provider_weights.is_empty() {
        let mut weighted_providers = Vec::new();
        for (provider, weight) in &pool.provider_weights {
            if ready_indices.iter().any(|&idx| {
                pool.placements
                    .get(idx)
                    .is_some_and(|p| p.provider == *provider)
            }) {
                for _ in 0..*weight {
                    weighted_providers.push(*provider);
                }
            }
        }
        if !weighted_providers.is_empty() {
            let slot = usize::try_from(*cursor).unwrap_or(0) % weighted_providers.len();
            *cursor = cursor.wrapping_add(1);
            if let Some(&chosen_provider) = weighted_providers.get(slot) {
                let provider_ready: Vec<usize> = ready_indices
                    .iter()
                    .copied()
                    .filter(|&idx| {
                        pool.placements
                            .get(idx)
                            .is_some_and(|p| p.provider == chosen_provider)
                    })
                    .collect();
                if pool.prefer_cheapest {
                    return provider_ready.first().copied();
                }
                if !provider_ready.is_empty() {
                    let sub_slot = usize::try_from(*cursor).unwrap_or(0) % provider_ready.len();
                    return provider_ready.get(sub_slot).copied();
                }
            }
        }
    }

    if pool.prefer_cheapest {
        return ready_indices.first().copied();
    }

    let len = pool.placements.len();
    let start = usize::try_from(*cursor).unwrap_or(0) % len;
    for offset in 0..len {
        let idx = (start + offset) % len;
        if ready_indices.contains(&idx) {
            *cursor = u64::try_from((idx + 1) % len).unwrap_or(0);
            return Some(idx);
        }
    }
    None
}

/// Counts queued jobs and unassigned active workers for scaling math.
pub fn demand_summary(workers: &[Worker], jobs: &[Job]) -> (usize, usize, usize) {
    let active_workers = workers.iter().filter(|w| w.phase.is_active()).count();
    let unassigned_workers = workers
        .iter()
        .filter(|w| {
            matches!(
                w.phase,
                WorkerPhase::Planned | WorkerPhase::Creating | WorkerPhase::Live
            ) && w.job_id.is_none()
        })
        .count();
    let queued_jobs = jobs
        .iter()
        .filter(|j| j.status == JobStatus::Queued)
        .count();
    (active_workers, unassigned_workers, queued_jobs)
}

/// Computes a per-provider active worker count map for metrics and UI summaries.
pub fn active_workers_by_provider(workers: &[Worker]) -> BTreeMap<ProviderKind, usize> {
    let mut counts = BTreeMap::new();
    for worker in workers.iter().filter(|w| w.phase.is_active()) {
        *counts.entry(worker.provider).or_insert(0) += 1;
    }
    counts
}
