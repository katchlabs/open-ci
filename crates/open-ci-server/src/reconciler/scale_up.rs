//! Pool scale-up planning, worker launch, and regional quota backoff recording.

use super::ReconcileSummary;
use crate::bootstrap::render_user_data;
use crate::{ControlPlane, ServerError};
use futures_util::stream::{self, StreamExt};
use open_ci_core::config::{Placement, PoolConfig};
use open_ci_core::reconcile::{
    RetireReason, demand_summary, plan_scale_up, select_placement, transition_phase,
};
use open_ci_core::{
    ControlEvent, ControlMode, Job, JobStatus, PlacementBackoff, Worker, WorkerPhase,
};
use open_ci_provider::{InstanceSpec, ProviderError};
use uuid::Uuid;

const QUOTA_BACKOFF_SECONDS: u64 = 300;
const CAPACITY_BACKOFF_SECONDS: u64 = 90;
const MAX_CONCURRENT_LAUNCHES: usize = 8;

struct PlannedLaunch {
    pool_id: String,
    disk_gb: u32,
    lifetime_seconds: u64,
    cache_url: Option<String>,
    placement: Placement,
    worker: Worker,
    now: u64,
}

#[derive(Clone, Copy, Debug)]
enum LaunchOutcome {
    Created,
    Retired,
}

impl ControlPlane {
    pub(super) async fn scale_up_pool(
        &self,
        pool_id: &str,
        pool: &PoolConfig,
        latest_jobs: &[Job],
        mode: ControlMode,
        now: u64,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        let current_workers = self.store.list_workers(Some(pool_id)).await?;
        let total_launched = self.store.count_pool_workers(pool_id).await?;
        let (active_workers, unassigned_workers, queued_jobs) =
            demand_summary(&current_workers, latest_jobs);
        let to_create = plan_scale_up(
            pool,
            mode,
            active_workers,
            total_launched,
            queued_jobs,
            unassigned_workers,
        );

        if to_create == 0 {
            return Ok(());
        }

        let backoffs = self.store.list_active_backoffs(now).await?;
        let mut cursor = self.store.get_routing_cursor(pool_id).await?;
        let mut assigned_by_admission = count_unassigned_by_admission(&current_workers);
        let mut planned = Vec::with_capacity(to_create);

        for _ in 0..to_create {
            let Some(placement_idx) = select_placement(pool, &backoffs, &mut cursor, now) else {
                break;
            };
            let Some(placement) = pool.placements.get(placement_idx) else {
                break;
            };
            self.store.set_routing_cursor(pool_id, cursor).await?;
            let admission_id = next_starved_admission(latest_jobs, &assigned_by_admission);
            if let Some(ref adm) = admission_id {
                *assigned_by_admission.entry(adm.clone()).or_insert(0) += 1;
            }

            let short_id = &Uuid::new_v4().simple().to_string()[..12];
            let worker = Worker {
                name: format!("open-ci-{pool_id}-{short_id}"),
                pool_id: pool_id.to_owned(),
                admission_id,
                request_id: Uuid::new_v4().to_string(),
                provider: placement.provider,
                placement_index: placement_idx,
                region: placement.region.clone(),
                zone: placement.zone.clone(),
                machine_type: placement.machine_type.clone(),
                provider_instance_id: None,
                phase: WorkerPhase::Planned,
                create_operation: None,
                delete_operation: None,
                delete_attempts: 0,
                bootstrap_used: false,
                runner_id: None,
                job_id: None,
                retire_reason: None,
                created_at: now,
                updated_at: now,
            };

            self.store.upsert_worker(&worker).await?;
            planned.push(PlannedLaunch {
                pool_id: pool_id.to_owned(),
                disk_gb: pool.disk_gb,
                lifetime_seconds: pool.lifetime_seconds,
                cache_url: self.config.worker_cache_url(pool).map(str::to_owned),
                placement: placement.clone(),
                worker,
                now,
            });
        }

        let results: Vec<_> = stream::iter(planned)
            .map(|launch| self.execute_planned_launch(launch))
            .buffer_unordered(MAX_CONCURRENT_LAUNCHES)
            .collect()
            .await;

        for result in results {
            match result {
                Ok(LaunchOutcome::Created) => summary.workers_created += 1,
                Ok(LaunchOutcome::Retired) => summary.workers_retired += 1,
                Err(err) => {
                    tracing::error!(pool = %pool_id, error = %err, "worker launch failed");
                }
            }
        }
        Ok(())
    }

    async fn execute_planned_launch(
        &self,
        launch: PlannedLaunch,
    ) -> Result<LaunchOutcome, ServerError> {
        let PlannedLaunch {
            pool_id,
            disk_gb,
            lifetime_seconds,
            cache_url,
            placement,
            mut worker,
            now,
        } = launch;

        let user_data = render_user_data(
            &self.secrets.bootstrap_secret,
            &self.config.server.public_url,
            &worker.name,
            lifetime_seconds,
            cache_url.as_deref(),
        )?;
        let provider = self.providers.get(placement.provider)?;
        let spec = InstanceSpec {
            worker_name: worker.name.clone(),
            pool_id: pool_id.clone(),
            request_id: worker.request_id.clone(),
            placement: placement.clone(),
            disk_gb,
            lifetime_seconds,
            expires_at_unix: now.saturating_add(lifetime_seconds),
            user_data,
            tenant_id: None,
        };

        match provider.create_instance(&spec).await {
            Ok(receipt) => {
                worker.provider_instance_id = receipt.provider_instance_id;
                worker.create_operation = receipt.operation.map(|o| o.0);
                let next_phase = if worker.create_operation.is_some() {
                    WorkerPhase::Creating
                } else {
                    WorkerPhase::Live
                };
                let _ = transition_phase(&mut worker, next_phase, now);
                self.store.upsert_worker(&worker).await?;
                self.emit(ControlEvent::WorkerChanged {
                    worker: worker.name,
                    pool_id,
                    phase: worker.phase,
                    timestamp: now,
                });
                Ok(LaunchOutcome::Created)
            }
            Err(err) => {
                self.record_provider_backoff(&placement, &err, now).await;
                worker.retire_reason = Some(RetireReason::CreateFailed.as_str().to_owned());
                let _ = transition_phase(&mut worker, WorkerPhase::Retiring, now);
                self.store.upsert_worker(&worker).await?;
                Ok(LaunchOutcome::Retired)
            }
        }
    }

    pub(super) async fn record_provider_backoff(
        &self,
        placement: &Placement,
        err: &ProviderError,
        now: u64,
    ) {
        let backoff = match err {
            ProviderError::QuotaExceeded { .. } => PlacementBackoff::regional(
                placement,
                now.saturating_add(QUOTA_BACKOFF_SECONDS),
                "quota_exceeded",
            ),
            ProviderError::CapacityUnavailable { .. } => PlacementBackoff::zonal(
                placement,
                now.saturating_add(CAPACITY_BACKOFF_SECONDS),
                "capacity_unavailable",
            ),
            ProviderError::RateLimited {
                retry_after_seconds,
            } => PlacementBackoff::regional(
                placement,
                now.saturating_add(*retry_after_seconds),
                "rate_limited",
            ),
            _ => return,
        };
        let retry_after = backoff.retry_after;
        if self.store.upsert_backoff(&backoff).await.is_ok() {
            self.emit(ControlEvent::BackoffTriggered {
                provider: backoff.provider,
                region: backoff.region,
                family: backoff.family,
                retry_after,
            });
        }
    }
}

fn count_unassigned_by_admission(workers: &[Worker]) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for worker in workers {
        if worker.phase.is_active()
            && worker.job_id.is_none()
            && let Some(adm) = &worker.admission_id
        {
            *counts.entry(adm.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn next_starved_admission(
    latest_jobs: &[Job],
    assigned_by_admission: &std::collections::BTreeMap<String, usize>,
) -> Option<String> {
    let mut queued_by_admission: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for job in latest_jobs {
        if job.status == JobStatus::Queued
            && let Some(adm) = job.admission_id.as_deref()
        {
            *queued_by_admission.entry(adm).or_insert(0) += 1;
        }
    }
    for (adm, queued) in queued_by_admission {
        let assigned = assigned_by_admission.get(adm).copied().unwrap_or(0);
        if queued > assigned {
            return Some(adm.to_owned());
        }
    }
    None
}
