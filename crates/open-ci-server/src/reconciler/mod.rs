//! Bounded, non-blocking asynchronous reconciliation engine.
//!
//! Reads durable snapshots from [`open_ci_store::Store`], executes bounded
//! GitHub and cloud API requests without holding any global mutex, applies
//! pure transitions from [`open_ci_core::reconcile`], and persists atomically.

mod orphans;
mod scale_up;
mod worker_lifecycle;

use crate::{ControlPlane, ServerError};
use open_ci_core::config::PoolConfig;
use open_ci_core::{ControlEvent, ControlMode, Job, JobStatus};
use std::collections::BTreeMap;

/// Summary returned by a single reconciliation pass.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ReconcileSummary {
    pub workers_created: usize,
    pub workers_retired: usize,
    pub workers_cleaned: usize,
    pub orphans_discovered: usize,
    pub admissions_released: usize,
}

impl ControlPlane {
    /// Runs one full reconciliation pass across all configured pools and held admissions.
    pub async fn reconcile_once(&self) -> Result<ReconcileSummary, ServerError> {
        let mut summary = ReconcileSummary::default();
        let control = self.store.control_state().await?;

        self.refresh_held_admission_jobs().await;

        let pool_ids: Vec<String> = self.config.pools.keys().cloned().collect();
        for pool_id in pool_ids {
            if let Some(pool) = self.config.pools.get(&pool_id).cloned() {
                self.reconcile_pool(&pool_id, &pool, control.mode, &mut summary)
                    .await?;
            }
        }

        summary.admissions_released = self.reconcile_held_admissions().await?;
        Ok(summary)
    }

    async fn refresh_held_admission_jobs(&self) {
        let Ok(held) = self.store.list_held_admissions().await else {
            return;
        };
        let now = self.clock.now_unix();
        for admission in held {
            let Ok(gh_jobs) = self
                .github
                .list_run_attempt_jobs(
                    Some(&admission.repository),
                    admission.run_id,
                    admission.run_attempt,
                )
                .await
            else {
                continue;
            };
            for gh_job in gh_jobs {
                let matched_pool = self.config.pools.iter().find_map(|(pool_id, pool)| {
                    let target = admission.runners.get(pool_id).map(String::as_str);
                    if let Some(label) = target {
                        pool.accepts_labels(&gh_job.labels, Some(label))
                            .then(|| pool_id.clone())
                    } else {
                        let scoped = format!("{}-{}", pool.runner_label, admission.id);
                        pool.accepts_labels(&gh_job.labels, Some(&scoped))
                            .then(|| pool_id.clone())
                    }
                });
                let Some(pool_id) = matched_pool else {
                    continue;
                };
                let job = Job {
                    id: gh_job.id,
                    pool_id: pool_id.clone(),
                    repository: Some(admission.repository.clone()),
                    admission_id: Some(admission.id.clone()),
                    run_id: Some(gh_job.run_id),
                    status: gh_job.parsed_status(),
                    conclusion: gh_job.conclusion,
                    runner_name: gh_job.runner_name,
                    runner_id: gh_job.runner_id,
                    checked_at: Some(now),
                    updated_at: now,
                };
                let _ = self.store.upsert_job(&job).await;
                self.emit(ControlEvent::JobChanged {
                    job_id: job.id,
                    pool_id,
                    status: job.status,
                    timestamp: now,
                });
            }
        }
    }

    async fn refresh_pool_jobs(
        &self,
        pool_id: &str,
        pool: &PoolConfig,
        now: u64,
    ) -> Result<(), ServerError> {
        let jobs = self.store.list_jobs(Some(pool_id)).await?;
        let scope = self.config.github_scope();
        for job in jobs {
            let due = job.status != JobStatus::Completed
                && job
                    .checked_at
                    .is_none_or(|t| now >= t.saturating_add(pool.job_poll_seconds));
            if !due {
                continue;
            }
            let repo = job
                .repository
                .as_deref()
                .or_else(|| scope.default_repository());
            if let Ok(Some(gh_job)) = self.github.get_job(repo, job.id).await {
                let updated = Job {
                    status: gh_job.parsed_status(),
                    conclusion: gh_job.conclusion,
                    runner_name: gh_job.runner_name,
                    runner_id: gh_job.runner_id,
                    checked_at: Some(now),
                    updated_at: now,
                    ..job
                };
                self.store.upsert_job(&updated).await?;
                self.emit(ControlEvent::JobChanged {
                    job_id: updated.id,
                    pool_id: pool_id.to_owned(),
                    status: updated.status,
                    timestamp: now,
                });
            }
        }
        Ok(())
    }

    async fn reconcile_pool(
        &self,
        pool_id: &str,
        pool: &PoolConfig,
        mode: ControlMode,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        let now = self.clock.now_unix();
        self.refresh_pool_jobs(pool_id, pool, now).await?;

        let last_discovery = self.store.get_last_discovery(pool_id).await?;
        let discovery_due = mode == ControlMode::Draining
            || now >= last_discovery.saturating_add(pool.cloud_discovery_seconds);
        let mut inventory_verified = true;
        if discovery_due {
            match self.discover_orphans(pool_id, pool, now).await {
                Ok(found) => {
                    summary.orphans_discovered += found;
                    self.store.set_last_discovery(pool_id, now).await?;
                }
                Err(err) => {
                    tracing::warn!(pool = %pool_id, error = %err, "cloud discovery failed; pausing scale-up");
                    inventory_verified = false;
                }
            }
        }

        let latest_jobs = self.store.list_jobs(Some(pool_id)).await?;
        let jobs_by_runner: BTreeMap<&str, &Job> = latest_jobs
            .iter()
            .filter_map(|j| j.runner_name.as_deref().map(|r| (r, j)))
            .collect();

        let gh_runners = self.github.list_runners().await.unwrap_or_default();
        let runners_by_name: BTreeMap<&str, u64> =
            gh_runners.iter().map(|r| (r.name.as_str(), r.id)).collect();

        let workers = self.store.list_workers(Some(pool_id)).await?;
        for mut worker in workers.into_iter().filter(|w| w.phase.is_active()) {
            if let Some(&rid) = runners_by_name.get(worker.name.as_str()) {
                worker.runner_id = Some(rid);
            }
            let assigned = jobs_by_runner.get(worker.name.as_str()).copied();
            if let Some(job) = assigned {
                worker.job_id = Some(job.id);
            }
            self.advance_worker(pool, &mut worker, assigned, mode, summary)
                .await?;
        }

        if inventory_verified {
            self.scale_up_pool(pool_id, pool, &latest_jobs, mode, now, summary)
                .await?;
        }
        Ok(())
    }

    async fn reconcile_held_admissions(&self) -> Result<usize, ServerError> {
        let held = self.store.list_held_admissions().await?;
        if held.is_empty() {
            return Ok(0);
        }
        let all_workers = self.store.list_workers(None).await?;
        let now = self.clock.now_unix();
        let mut released = 0;

        for admission in held {
            let has_active_workers = all_workers
                .iter()
                .any(|w| w.admission_id.as_deref() == Some(&admission.id) && w.phase.is_active());
            if has_active_workers {
                continue;
            }
            let expired = now
                >= admission
                    .created_at
                    .saturating_add(self.config.admission.max_run_seconds);
            let run_completed = match self
                .github
                .get_workflow_run(Some(&admission.repository), admission.run_id)
                .await
            {
                Ok(Some(run)) => run.status == "completed",
                Ok(None) => true,
                Err(_) => false,
            };
            if expired || run_completed {
                self.store.release_admission(&admission.id, now).await?;
                self.emit(ControlEvent::AdmissionChanged {
                    reservation: admission.id,
                    run_id: admission.run_id,
                    status: open_ci_core::AdmissionStatus::Released,
                    reason: admission.reason,
                    timestamp: now,
                });
                released += 1;
            }
        }
        Ok(released)
    }
}
