//! Pool scale-up planning, worker launch, and regional quota backoff recording.

use super::ReconcileSummary;
use crate::bootstrap::{CacheBootstrap, render_user_data};
use crate::{ControlPlane, ServerError};
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

struct LaunchRequest<'a> {
    pool_id: &'a str,
    pool: &'a PoolConfig,
    placement_idx: usize,
    placement: &'a Placement,
    admission_id: Option<String>,
    now: u64,
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
            let req = LaunchRequest {
                pool_id,
                pool,
                placement_idx,
                placement,
                admission_id,
                now,
            };
            self.launch_single_worker(req, summary).await?;
        }
        Ok(())
    }

    async fn launch_single_worker(
        &self,
        req: LaunchRequest<'_>,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        let LaunchRequest {
            pool_id,
            pool,
            placement_idx,
            placement,
            admission_id,
            now,
        } = req;
        let short_id = &Uuid::new_v4().simple().to_string()[..12];
        let worker_name = format!("open-ci-{pool_id}-{short_id}");
        let request_id = Uuid::new_v4().to_string();

        let mut worker = Worker {
            name: worker_name,
            pool_id: pool_id.to_owned(),
            admission_id,
            request_id: request_id.clone(),
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

        // Persist Planned intent BEFORE calling the cloud API so crashes remain idempotent.
        self.store.upsert_worker(&worker).await?;

        let cache_bootstrap = self
            .build_cache_bootstrap(worker.admission_id.as_deref(), pool.lifetime_seconds, now)
            .await?;
        let user_data = render_user_data(
            &self.secrets.bootstrap_secret,
            &self.config.server.public_url,
            &worker.name,
            pool.lifetime_seconds,
            cache_bootstrap,
        )?;
        let provider = self.providers.get(placement.provider)?;
        let spec = InstanceSpec {
            worker_name: worker.name.clone(),
            pool_id: pool_id.to_owned(),
            request_id,
            placement: placement.clone(),
            disk_gb: pool.disk_gb,
            lifetime_seconds: pool.lifetime_seconds,
            expires_at_unix: now.saturating_add(pool.lifetime_seconds),
            user_data,
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
                    pool_id: pool_id.to_owned(),
                    phase: worker.phase,
                    timestamp: now,
                });
                summary.workers_created += 1;
            }
            Err(err) => {
                self.record_provider_backoff(placement, &err, now).await;
                worker.retire_reason = Some(RetireReason::CreateFailed.as_str().to_owned());
                let _ = transition_phase(&mut worker, WorkerPhase::Retiring, now);
                self.store.upsert_worker(&worker).await?;
                summary.workers_retired += 1;
            }
        }
        Ok(())
    }

    async fn build_cache_bootstrap(
        &self,
        admission_id: Option<&str>,
        lifetime_seconds: u64,
        now: u64,
    ) -> Result<Option<CacheBootstrap<'_>>, ServerError> {
        let Some(cache_cfg) = &self.config.cache else {
            return Ok(None);
        };
        let token = match &self.secrets.cache_signing_key {
            Some(key) => {
                let scope = match admission_id {
                    Some(id) => self.store.get_admission(id).await?.map_or_else(
                        || self.config.github_scope().display_target().to_owned(),
                        |a| a.repository,
                    ),
                    None => self.config.github_scope().display_target().to_owned(),
                };
                let minted = open_ci_cache::issue_cache_token(key, &scope, lifetime_seconds, now)
                    .map_err(|err| {
                    ServerError::InvalidConfig(format!("cache token error: {err}"))
                })?;
                Some(minted)
            }
            None => None,
        };
        Ok(Some(CacheBootstrap {
            url: &cache_cfg.public_url,
            token,
        }))
    }

    pub(super) async fn record_provider_backoff(
        &self,
        placement: &Placement,
        err: &ProviderError,
        now: u64,
    ) {
        let (duration, reason) = match err {
            ProviderError::QuotaExceeded { .. } => (QUOTA_BACKOFF_SECONDS, "quota_exceeded"),
            ProviderError::CapacityUnavailable { .. } => {
                (CAPACITY_BACKOFF_SECONDS, "capacity_unavailable")
            }
            ProviderError::RateLimited {
                retry_after_seconds,
            } => (*retry_after_seconds, "rate_limited"),
            _ => return,
        };
        let retry_after = now.saturating_add(duration);
        let backoff = PlacementBackoff {
            provider: placement.provider,
            region: placement.region.clone(),
            family: placement.machine_family().to_owned(),
            retry_after,
            reason: reason.to_owned(),
        };
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
