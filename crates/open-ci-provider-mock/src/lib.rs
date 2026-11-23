//! Deterministic, fault-injectable in-memory cloud provider for tests and `open-ci demo`.

use async_trait::async_trait;
use open_ci_core::ProviderKind;
use open_ci_core::config::Placement;
use open_ci_provider::{
    CloudDisk, CloudInstance, CloudProvider, CreateReceipt, InstanceSpec, InstanceStatus,
    OperationRef, OperationStatus, ProviderError,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default, Debug)]
struct MockState {
    instances: BTreeMap<String, CloudInstance>,
    disks: BTreeMap<String, CloudDisk>,
    request_ids: BTreeMap<String, String>,
    faults: VecDeque<ProviderError>,
    healthy: bool,
    op_counter: u64,
}

/// Thread-safe in-memory [`CloudProvider`] supporting deterministic fault injection.
#[derive(Clone, Debug)]
pub struct MockProvider {
    kind: ProviderKind,
    state: Arc<Mutex<MockState>>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new(ProviderKind::Mock)
    }
}

impl MockProvider {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            state: Arc::new(Mutex::new(MockState {
                healthy: true,
                ..MockState::default()
            })),
        }
    }

    pub async fn set_healthy(&self, healthy: bool) {
        self.state.lock().await.healthy = healthy;
    }

    pub async fn inject_create_fault(&self, error: ProviderError) {
        self.state.lock().await.faults.push_back(error);
    }

    pub async fn preempt_instance(&self, name: &str) {
        let mut guard = self.state.lock().await;
        if let Some(inst) = guard.instances.get_mut(name) {
            inst.status = InstanceStatus::Terminated;
        }
    }

    pub async fn inject_orphan(&self, instance: CloudInstance, disk: Option<CloudDisk>) {
        let mut guard = self.state.lock().await;
        guard.instances.insert(instance.name.clone(), instance);
        if let Some(d) = disk {
            guard.disks.insert(d.name.clone(), d);
        }
    }

    pub async fn counts(&self) -> (usize, usize) {
        let guard = self.state.lock().await;
        (guard.instances.len(), guard.disks.len())
    }
}

#[async_trait]
impl CloudProvider for MockProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn health_check(&self, _placement: &Placement) -> Result<(), ProviderError> {
        if self.state.lock().await.healthy {
            Ok(())
        } else {
            Err(ProviderError::Transient(
                "mock provider unhealthy".to_owned(),
            ))
        }
    }

    async fn get_instance(
        &self,
        _placement: &Placement,
        worker_name: &str,
        _provider_instance_id: Option<&str>,
    ) -> Result<Option<CloudInstance>, ProviderError> {
        Ok(self.state.lock().await.instances.get(worker_name).cloned())
    }

    async fn list_instances(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudInstance>, ProviderError> {
        let guard = self.state.lock().await;
        Ok(guard
            .instances
            .values()
            .filter(|i| i.pool_id == pool_id && i.zone == placement.zone)
            .cloned()
            .collect())
    }

    async fn list_disks(
        &self,
        _placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError> {
        let guard = self.state.lock().await;
        Ok(guard
            .disks
            .values()
            .filter(|d| d.pool_id == pool_id)
            .cloned()
            .collect())
    }

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError> {
        let mut guard = self.state.lock().await;
        if let Some(fault) = guard.faults.pop_front() {
            return Err(fault);
        }
        if let Some(existing_id) = guard.request_ids.get(&spec.request_id).cloned() {
            return Ok(CreateReceipt {
                provider_instance_id: Some(existing_id),
                operation: None,
            });
        }
        guard.op_counter += 1;
        let op_name = format!("mock-op-{}", guard.op_counter);
        let provider_id = format!("mock-vm-{}", guard.op_counter);
        guard
            .request_ids
            .insert(spec.request_id.clone(), provider_id.clone());
        guard.instances.insert(
            spec.worker_name.clone(),
            CloudInstance {
                provider_id: provider_id.clone(),
                name: spec.worker_name.clone(),
                pool_id: spec.pool_id.clone(),
                status: InstanceStatus::Running,
                zone: spec.placement.zone.clone(),
            },
        );
        let disk_id = format!("mock-disk-{}", guard.op_counter);
        guard.disks.insert(
            spec.worker_name.clone(),
            CloudDisk {
                provider_id: disk_id,
                name: spec.worker_name.clone(),
                pool_id: spec.pool_id.clone(),
                attached: true,
                status: "READY".to_owned(),
            },
        );
        Ok(CreateReceipt {
            provider_instance_id: Some(provider_id),
            operation: Some(OperationRef(op_name)),
        })
    }

    async fn poll_operation(
        &self,
        _placement: &Placement,
        _operation: &OperationRef,
    ) -> Result<OperationStatus, ProviderError> {
        Ok(OperationStatus::Done)
    }

    async fn delete_instance(
        &self,
        _placement: &Placement,
        worker_name: &str,
        _provider_instance_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        let mut guard = self.state.lock().await;
        let existed = guard.instances.remove(worker_name).is_some();
        if let Some(disk) = guard.disks.get_mut(worker_name) {
            disk.attached = false;
        }
        if existed {
            guard.op_counter += 1;
            Ok(Some(OperationRef(format!("mock-del-{}", guard.op_counter))))
        } else {
            Ok(None)
        }
    }

    async fn delete_disk(
        &self,
        _placement: &Placement,
        disk_name: &str,
        _provider_disk_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        let mut guard = self.state.lock().await;
        let existed = guard.disks.remove(disk_name).is_some();
        if existed {
            guard.op_counter += 1;
            Ok(Some(OperationRef(format!(
                "mock-disk-del-{}",
                guard.op_counter
            ))))
        } else {
            Ok(None)
        }
    }
}
