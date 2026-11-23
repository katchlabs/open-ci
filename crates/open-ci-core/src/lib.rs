//! Pure domain core for `open-ci`.
//!
//! Contains configuration schemas, domain models, capacity accounting,
//! admission evaluation, and worker lifecycle transition logic with zero
//! network, database, or filesystem side effects.

pub mod admission;
pub mod clock;
pub mod config;
pub mod error;
pub mod models;
pub mod reconcile;
pub mod secret;

pub use clock::{Clock, ManualClock, SystemClock};
pub use config::OpenCiConfig;
pub use error::{ConfigError, DomainError};
pub use models::{
    AdmissionDecision, AdmissionMode, AdmissionReason, AdmissionRecord, AdmissionRequest,
    AdmissionStatus, Architecture, AuditEntry, Capacity, ControlEvent, ControlMode, GitHubScope,
    Job, JobStatus, PlacementBackoff, ProviderKind, Worker, WorkerPhase, WorkerUsage,
};
pub use secret::SecretString;
