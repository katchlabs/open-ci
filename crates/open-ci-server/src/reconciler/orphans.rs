//! Cloud orphan discovery across all placements of a pool.

use crate::{ControlPlane, ServerError};
use open_ci_core::config::PoolConfig;
use open_ci_core::{Worker, WorkerPhase};
use std::collections::BTreeMap;
use uuid::Uuid;

impl ControlPlane {
    pub(super) async fn discover_orphans(
        &self,
        pool_id: &str,
        pool: &PoolConfig,
        now: u64,
    ) -> Result<usize, ServerError> {
        let mut discovered = 0;
        let known: BTreeMap<String, Worker> = self
            .store
            .list_workers(Some(pool_id))
            .await?
            .into_iter()
            .map(|w| (w.name.clone(), w))
            .collect();

        for (placement_idx, placement) in pool.placements.iter().enumerate() {
            let provider = self.providers.get(placement.provider)?;
            let instances = provider.list_instances(placement, pool_id).await?;
            for inst in instances {
                if known.contains_key(&inst.name) {
                    continue;
                }
                let orphan = Worker {
                    name: inst.name,
                    pool_id: pool_id.to_owned(),
                    admission_id: None,
                    request_id: format!("orphan-{}", Uuid::new_v4()),
                    provider: placement.provider,
                    placement_index: placement_idx,
                    region: placement.region.clone(),
                    zone: placement.zone.clone(),
                    machine_type: placement.machine_type.clone(),
                    provider_instance_id: Some(inst.provider_id),
                    phase: WorkerPhase::Retiring,
                    create_operation: None,
                    delete_operation: None,
                    delete_attempts: 0,
                    bootstrap_used: true,
                    runner_id: None,
                    job_id: None,
                    retire_reason: Some("orphan_discovered".to_owned()),
                    created_at: now,
                    updated_at: now,
                };
                self.store.upsert_worker(&orphan).await?;
                discovered += 1;
            }
        }

        Ok(discovered)
    }
}
