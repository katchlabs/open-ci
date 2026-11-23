//! Per-worker lifecycle state machine transitions (`Planned -> Creating -> Live -> Retiring -> Gone`).

use super::ReconcileSummary;
use crate::{ControlPlane, ServerError};
use open_ci_core::config::{Placement, PoolConfig};
use open_ci_core::reconcile::{
    RetireReason, evaluate_retirement, missing_instance_is_lost, transition_phase,
};
use open_ci_core::{ControlEvent, ControlMode, Job, Worker, WorkerPhase};
use open_ci_provider::{CloudProvider, InstanceStatus, OperationRef, OperationStatus};

struct LiveContext<'a> {
    pool: &'a PoolConfig,
    placement: &'a Placement,
    provider: &'a dyn CloudProvider,
    assigned_job: Option<&'a Job>,
    mode: ControlMode,
    now: u64,
}

impl ControlPlane {
    pub(super) async fn advance_worker(
        &self,
        pool: &PoolConfig,
        worker: &mut Worker,
        assigned_job: Option<&Job>,
        mode: ControlMode,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        let now = self.clock.now_unix();
        let Some(placement) = pool.placements.get(worker.placement_index) else {
            return Ok(());
        };
        let provider = self.providers.get(placement.provider)?;

        match worker.phase {
            WorkerPhase::Planned => {
                self.advance_planned(pool, placement, provider.as_ref(), worker, now)
                    .await
            }
            WorkerPhase::Creating => {
                self.advance_creating(placement, provider.as_ref(), worker, now, summary)
                    .await
            }
            WorkerPhase::Live => {
                let ctx = LiveContext {
                    pool,
                    placement,
                    provider: provider.as_ref(),
                    assigned_job,
                    mode,
                    now,
                };
                self.advance_live(&ctx, worker, summary).await
            }
            WorkerPhase::Retiring => {
                self.advance_retiring(placement, provider.as_ref(), worker, now, summary)
                    .await
            }
            WorkerPhase::Gone => Ok(()),
        }
    }

    async fn advance_planned(
        &self,
        pool: &PoolConfig,
        placement: &Placement,
        provider: &dyn CloudProvider,
        worker: &mut Worker,
        now: u64,
    ) -> Result<(), ServerError> {
        if let Some(inst) = provider
            .get_instance(
                placement,
                &worker.name,
                worker.provider_instance_id.as_deref(),
            )
            .await?
        {
            worker.provider_instance_id = Some(inst.provider_id);
            let next = match inst.status {
                InstanceStatus::Running => WorkerPhase::Live,
                InstanceStatus::Terminated | InstanceStatus::Stopping => {
                    worker.retire_reason =
                        Some(RetireReason::InstanceTerminated.as_str().to_owned());
                    WorkerPhase::Retiring
                }
                InstanceStatus::Provisioning => WorkerPhase::Creating,
            };
            let _ = transition_phase(worker, next, now);
        } else if now >= worker.created_at.saturating_add(pool.idle_seconds) {
            worker.retire_reason = Some(RetireReason::CreateFailed.as_str().to_owned());
            let _ = transition_phase(worker, WorkerPhase::Retiring, now);
        }
        self.store.upsert_worker(worker).await?;
        Ok(())
    }

    async fn advance_creating(
        &self,
        placement: &Placement,
        provider: &dyn CloudProvider,
        worker: &mut Worker,
        now: u64,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        if let Some(op_name) = worker.create_operation.clone() {
            match provider
                .poll_operation(placement, &OperationRef(op_name))
                .await?
            {
                OperationStatus::Pending => {}
                OperationStatus::Done => {
                    worker.create_operation = None;
                    let _ = transition_phase(worker, WorkerPhase::Live, now);
                    self.emit(ControlEvent::WorkerChanged {
                        worker: worker.name.clone(),
                        pool_id: worker.pool_id.clone(),
                        phase: WorkerPhase::Live,
                        timestamp: now,
                    });
                }
                OperationStatus::Failed(err) => {
                    self.record_provider_backoff(placement, &err, now).await;
                    worker.create_operation = None;
                    worker.retire_reason = Some(RetireReason::CreateFailed.as_str().to_owned());
                    let _ = transition_phase(worker, WorkerPhase::Retiring, now);
                    summary.workers_retired += 1;
                }
            }
        } else {
            let _ = transition_phase(worker, WorkerPhase::Live, now);
        }
        self.store.upsert_worker(worker).await?;
        Ok(())
    }

    async fn advance_live(
        &self,
        ctx: &LiveContext<'_>,
        worker: &mut Worker,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        let cloud_inst = ctx
            .provider
            .get_instance(
                ctx.placement,
                &worker.name,
                worker.provider_instance_id.as_deref(),
            )
            .await?;
        let preempted = cloud_inst.as_ref().map_or_else(
            || missing_instance_is_lost(worker, ctx.now),
            |i| {
                matches!(
                    i.status,
                    InstanceStatus::Terminated | InstanceStatus::Stopping
                )
            },
        );

        let reason = if preempted {
            Some(RetireReason::InstanceTerminated)
        } else {
            evaluate_retirement(ctx.pool, worker, ctx.assigned_job, ctx.mode, ctx.now)
        };

        if let Some(r) = reason {
            worker.retire_reason = Some(r.as_str().to_owned());
            let _ = transition_phase(worker, WorkerPhase::Retiring, ctx.now);
            self.store.upsert_worker(worker).await?;
            self.emit(ControlEvent::WorkerChanged {
                worker: worker.name.clone(),
                pool_id: worker.pool_id.clone(),
                phase: WorkerPhase::Retiring,
                timestamp: ctx.now,
            });
            summary.workers_retired += 1;
        } else {
            self.store.upsert_worker(worker).await?;
        }
        Ok(())
    }

    async fn advance_retiring(
        &self,
        placement: &Placement,
        provider: &dyn CloudProvider,
        worker: &mut Worker,
        now: u64,
        summary: &mut ReconcileSummary,
    ) -> Result<(), ServerError> {
        if let Some(inst) = provider
            .get_instance(
                placement,
                &worker.name,
                worker.provider_instance_id.as_deref(),
            )
            .await?
        {
            worker.delete_attempts = worker.delete_attempts.saturating_add(1);
            if let Some(op) = provider
                .delete_instance(placement, &worker.name, Some(&inst.provider_id))
                .await?
            {
                worker.delete_operation = Some(op.0);
            }
            worker.updated_at = now;
            self.store.upsert_worker(worker).await?;
            return Ok(());
        }

        let disks = provider.list_disks(placement, &worker.pool_id).await?;
        let mut disk_remaining = false;
        for disk in disks.into_iter().filter(|d| d.name == worker.name) {
            disk_remaining = true;
            if !disk.attached {
                let _ = provider
                    .delete_disk(placement, &disk.name, Some(&disk.provider_id))
                    .await?;
            }
        }
        if disk_remaining {
            worker.updated_at = now;
            self.store.upsert_worker(worker).await?;
            return Ok(());
        }

        if let Some(runner_id) = worker.runner_id {
            self.github.delete_runner(runner_id).await?;
            worker.runner_id = None;
        }

        let _ = transition_phase(worker, WorkerPhase::Gone, now);
        self.store.upsert_worker(worker).await?;
        self.emit(ControlEvent::WorkerChanged {
            worker: worker.name.clone(),
            pool_id: worker.pool_id.clone(),
            phase: WorkerPhase::Gone,
            timestamp: now,
        });
        summary.workers_cleaned += 1;
        Ok(())
    }
}
