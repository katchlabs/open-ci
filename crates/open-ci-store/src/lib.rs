//! Async storage port and normalized `SQLite` implementation for `open-ci`.

pub mod migrations;
pub mod sqlite;

pub use sqlite::SqliteStore;

use async_trait::async_trait;
use open_ci_core::{
    AdmissionRecord, AuditEntry, ControlMode, Job, PlacementBackoff, Worker, WorkerUsage,
};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("storage I/O failure: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("database is locked by another open-ci process")]
    AlreadyLocked,

    #[error("background storage worker failed: {0}")]
    Join(String),

    #[error("corrupt or unexpected stored value: {0}")]
    CorruptData(String),

    #[error("bootstrap capability already consumed or worker ineligible")]
    BootstrapUnavailable,
}

/// Durable control-plane state snapshot for admission decisions.
#[derive(Clone, Debug)]
pub struct ControlState {
    pub mode: ControlMode,
    pub updated_at: u64,
    pub updated_by: String,
    pub reason: String,
}

/// Async repository abstraction decouples the control plane from `SQLite`,
/// enabling future `PostgreSQL` or in-memory backends without business-logic churn.
#[async_trait]
pub trait Store: Send + Sync + std::fmt::Debug {
    async fn control_state(&self) -> Result<ControlState, StoreError>;
    async fn set_control_mode(
        &self,
        mode: ControlMode,
        actor: String,
        reason: String,
        now_unix: u64,
    ) -> Result<ControlState, StoreError>;

    async fn get_routing_cursor(&self, pool_id: &str) -> Result<u64, StoreError>;
    async fn set_routing_cursor(&self, pool_id: &str, cursor: u64) -> Result<(), StoreError>;
    async fn get_last_discovery(&self, pool_id: &str) -> Result<u64, StoreError>;
    async fn set_last_discovery(&self, pool_id: &str, timestamp: u64) -> Result<(), StoreError>;

    async fn get_admission(&self, id: &str) -> Result<Option<AdmissionRecord>, StoreError>;
    async fn get_admission_by_run(
        &self,
        repository: &str,
        run_id: u64,
        run_attempt: u32,
    ) -> Result<Option<AdmissionRecord>, StoreError>;
    async fn list_admissions(&self, limit: usize) -> Result<Vec<AdmissionRecord>, StoreError>;
    async fn list_held_admissions(&self) -> Result<Vec<AdmissionRecord>, StoreError>;
    async fn count_admitted(&self, utc_day_start: u64) -> Result<(u32, u32), StoreError>;
    async fn save_admission(&self, record: &AdmissionRecord) -> Result<(), StoreError>;
    async fn release_admission(&self, id: &str, released_at: u64) -> Result<(), StoreError>;

    async fn upsert_worker(&self, worker: &Worker) -> Result<(), StoreError>;
    async fn get_worker(&self, name: &str) -> Result<Option<Worker>, StoreError>;
    async fn list_workers(&self, pool_id: Option<&str>) -> Result<Vec<Worker>, StoreError>;
    async fn count_pool_workers(&self, pool_id: &str) -> Result<usize, StoreError>;

    /// Atomically burns the one-time bootstrap flag before calling GitHub's non-idempotent JIT API.
    async fn consume_bootstrap(&self, name: &str, now_unix: u64) -> Result<Worker, StoreError>;

    async fn observe_job_hint(
        &self,
        job_id: u64,
        pool_id: &str,
        repository: Option<&str>,
        admission_id: Option<&str>,
        run_id: Option<u64>,
        now_unix: u64,
    ) -> Result<(), StoreError>;
    async fn upsert_job(&self, job: &Job) -> Result<(), StoreError>;
    async fn list_jobs(&self, pool_id: Option<&str>) -> Result<Vec<Job>, StoreError>;

    async fn record_usage(
        &self,
        worker_name: &str,
        usage: WorkerUsage,
        observed_at: u64,
    ) -> Result<(), StoreError>;
    async fn list_usage(&self) -> Result<Vec<(String, WorkerUsage, u64)>, StoreError>;

    async fn upsert_backoff(&self, backoff: &PlacementBackoff) -> Result<(), StoreError>;
    async fn list_active_backoffs(
        &self,
        now_unix: u64,
    ) -> Result<Vec<PlacementBackoff>, StoreError>;

    async fn append_audit(
        &self,
        timestamp: u64,
        actor: &str,
        action: &str,
        target: &str,
        detail: serde_json::Value,
    ) -> Result<(), StoreError>;
    async fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>, StoreError>;

    /// Creates a consistent online backup snapshot without stopping the service.
    async fn backup_to(&self, destination: PathBuf) -> Result<(), StoreError>;
}
