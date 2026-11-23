//! Strongly-typed cloud provider boundary for `open-ci`.
//!
//! Every cloud adapter translates its wire format into these normalized types
//! at the adapter edge — no untyped provider JSON leaks into the control plane.

use async_trait::async_trait;
use open_ci_core::ProviderKind;
use open_ci_core::config::Placement;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

/// Normalized cloud instance lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Provisioning,
    Running,
    Stopping,
    Terminated,
}

/// Normalized cloud compute instance returned by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudInstance {
    pub provider_id: String,
    pub name: String,
    pub pool_id: String,
    pub status: InstanceStatus,
    pub zone: String,
}

/// Normalized cloud persistent/root disk returned by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudDisk {
    pub provider_id: String,
    pub name: String,
    pub pool_id: String,
    pub attached: bool,
    pub status: String,
}

/// Typed specification for launching a single disposable Spot runner VM.
#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub worker_name: String,
    pub pool_id: String,
    pub request_id: String,
    pub placement: Placement,
    pub disk_gb: u32,
    pub lifetime_seconds: u64,
    pub expires_at_unix: u64,
    pub user_data: String,
}

/// Reference to an asynchronous cloud operation (e.g., GCP zone operation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRef(pub String);

/// Receipt returned when a cloud provider accepts an instance creation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateReceipt {
    pub provider_instance_id: Option<String>,
    pub operation: Option<OperationRef>,
}

/// Status of an asynchronous cloud operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    Pending,
    Done,
    Failed(ProviderError),
}

/// Strongly-typed provider failure categories (safe to log; never embeds raw secrets/bodies).
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ProviderError {
    #[error("cloud quota exceeded in {region} for family {family}")]
    QuotaExceeded { region: String, family: String },

    #[error("spot capacity unavailable in {zone}")]
    CapacityUnavailable { zone: String },

    #[error("provider rate limited (retry after {retry_after_seconds}s)")]
    RateLimited { retry_after_seconds: u64 },

    #[error("provider resource not found")]
    NotFound,

    #[error("provider authentication or authorization failed")]
    AuthFailed,

    #[error("invalid provider configuration: {0}")]
    InvalidConfig(String),

    #[error("provider request timed out")]
    Timeout,

    #[error("transient provider failure: {0}")]
    Transient(String),
}

/// Uniform async interface implemented by every cloud compute adapter.
#[async_trait]
pub trait CloudProvider: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> ProviderKind;

    async fn health_check(&self, placement: &Placement) -> Result<(), ProviderError>;

    async fn get_instance(
        &self,
        placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<CloudInstance>, ProviderError>;

    async fn list_instances(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudInstance>, ProviderError>;

    async fn list_disks(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError>;

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError>;

    async fn poll_operation(
        &self,
        placement: &Placement,
        operation: &OperationRef,
    ) -> Result<OperationStatus, ProviderError>;

    async fn delete_instance(
        &self,
        placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError>;

    async fn delete_disk(
        &self,
        placement: &Placement,
        disk_name: &str,
        provider_disk_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError>;
}

/// Registry dispatching typed cloud operations to configured provider adapters.
#[derive(Clone, Default, Debug)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderKind, Arc<dyn CloudProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn CloudProvider>) {
        self.providers.insert(provider.kind(), provider);
    }

    pub fn get(&self, kind: ProviderKind) -> Result<Arc<dyn CloudProvider>, ProviderError> {
        self.providers.get(&kind).cloned().ok_or_else(|| {
            ProviderError::InvalidConfig(format!("provider `{kind}` not configured"))
        })
    }

    pub fn kinds(&self) -> Vec<ProviderKind> {
        self.providers.keys().copied().collect()
    }
}
