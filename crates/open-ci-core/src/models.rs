//! Vendor-neutral domain models for `open-ci`.

use crate::config::Placement;
use crate::error::DomainError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Supported cloud compute providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Gcp,
    Aws,
    #[serde(rename = "cloud_hypervisor")]
    CloudHypervisor,
    Mock,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gcp => "gcp",
            Self::Aws => "aws",
            Self::CloudHypervisor => "cloud_hypervisor",
            Self::Mock => "mock",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CPU architecture of a runner pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Architecture {
    X64,
    Arm64,
}

impl Architecture {
    /// GitHub Actions runner architecture label (`X64` or `ARM64`).
    pub const fn github_label(self) -> &'static str {
        match self {
            Self::X64 => "X64",
            Self::Arm64 => "ARM64",
        }
    }
}

/// Durable operating mode of the control plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    /// Admit new runs and provision runners normally.
    #[default]
    Active,
    /// Route new admissions to fallback runner labels while keeping existing pools reconciling.
    FallbackOnly,
    /// Stop launching new workers and retire idle workers until all active workers are cleaned up.
    Draining,
    /// Temporarily pause new admissions due to safety guard or operator action.
    Paused,
}

impl ControlMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::FallbackOnly => "fallback_only",
            Self::Draining => "draining",
            Self::Paused => "paused",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "fallback_only" => Some(Self::FallbackOnly),
            "draining" => Some(Self::Draining),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }

    pub const fn allows_new_admissions(self) -> bool {
        matches!(self, Self::Active)
    }

    pub const fn allows_new_workers(self) -> bool {
        matches!(self, Self::Active | Self::FallbackOnly | Self::Paused)
    }
}

/// How workflow runs are admitted to self-hosted runner capacity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    /// Explicit OIDC admission via `/v1/ingress/admit` (default, backwards compatible).
    #[default]
    Oidc,
    /// Automatic admission directly from signed `workflow_job` `queued` webhooks.
    Webhook,
    /// Supports both explicit OIDC admission and automatic direct webhook admission.
    Hybrid,
}

impl AdmissionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Webhook => "webhook",
            Self::Hybrid => "hybrid",
        }
    }

    pub const fn allows_oidc_admission(self) -> bool {
        matches!(self, Self::Oidc | Self::Hybrid)
    }

    pub const fn allows_webhook_admission(self) -> bool {
        matches!(self, Self::Webhook | Self::Hybrid)
    }
}

/// Target GitHub scope: either a single repository or an organization with a repo allowlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitHubScope {
    Repository(String),
    Organization {
        org: String,
        allowed_repositories: Vec<String>,
        runner_group_id: Option<u64>,
    },
}

impl GitHubScope {
    /// Returns `true` if `repo` is a valid `owner/name` permitted by this GitHub scope.
    pub fn allows_repository(&self, repo: &str) -> bool {
        let Some((owner, _)) = split_repository(repo) else {
            return false;
        };
        match self {
            Self::Repository(configured) => {
                split_repository(configured).is_some() && repo == configured
            }
            Self::Organization {
                org,
                allowed_repositories,
                ..
            } => {
                is_valid_github_segment(org)
                    && owner == org
                    && allowed_repositories
                        .iter()
                        .any(|pattern| repo_matches_pattern(repo, pattern))
            }
        }
    }

    /// Returns the single configured repository when repo-scoped, or `None` when org-scoped.
    pub fn default_repository(&self) -> Option<&str> {
        match self {
            Self::Repository(repo) => Some(repo.as_str()),
            Self::Organization { .. } => None,
        }
    }

    /// Returns the GitHub runner group ID used when creating JIT runner configs (defaults to `1`).
    pub const fn runner_group_id(&self) -> u64 {
        match self {
            Self::Organization {
                runner_group_id: Some(id),
                ..
            } => *id,
            Self::Repository(_)
            | Self::Organization {
                runner_group_id: None,
                ..
            } => 1,
        }
    }

    /// Human-readable scope target (`owner/repo` or `org`).
    pub fn display_target(&self) -> &str {
        match self {
            Self::Repository(repo) => repo.as_str(),
            Self::Organization { org, .. } => org.as_str(),
        }
    }

    pub const fn is_organization(&self) -> bool {
        matches!(self, Self::Organization { .. })
    }
}

/// Validates a single GitHub owner or repository name segment (forbidding path traversal).
pub fn is_valid_github_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 100
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Splits a validated `owner/name` repository identifier into `(owner, name)`.
pub fn split_repository(repo: &str) -> Option<(&str, &str)> {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None)
            if is_valid_github_segment(owner) && is_valid_github_segment(name) =>
        {
            Some((owner, name))
        }
        _ => None,
    }
}

/// Matches a validated `owner/name` repository against an exact or trailing-`*` pattern
/// such as `my-org/*`, `my-org/service-*`, or `my-org/api`.
pub fn repo_matches_pattern(repo: &str, pattern: &str) -> bool {
    let Some((repo_owner, repo_name)) = split_repository(repo) else {
        return false;
    };
    let mut parts = pattern.split('/');
    let (Some(pat_owner), Some(pat_name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if !is_valid_github_segment(pat_owner) || repo_owner != pat_owner {
        return false;
    }
    if pat_name == "*" {
        return true;
    }
    if let Some(prefix) = pat_name.strip_suffix('*') {
        return !prefix.is_empty()
            && !prefix.contains('*')
            && is_valid_github_segment(prefix)
            && repo_name.starts_with(prefix);
    }
    is_valid_github_segment(pat_name) && repo_name == pat_name
}

/// Lifecycle phase of an ephemeral cloud worker VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Intent persisted in the store; cloud create call has not been confirmed yet.
    Planned,
    /// Cloud create operation is in flight.
    Creating,
    /// Instance is running and eligible to bootstrap or execute a job.
    Live,
    /// Instance is being deleted along with its disk and GitHub runner registration.
    Retiring,
    /// VM, disk, and runner registration are all verified gone.
    Gone,
}

impl WorkerPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Creating => "creating",
            Self::Live => "live",
            Self::Retiring => "retiring",
            Self::Gone => "gone",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "planned" => Some(Self::Planned),
            "creating" => Some(Self::Creating),
            "live" => Some(Self::Live),
            "retiring" => Some(Self::Retiring),
            "gone" => Some(Self::Gone),
            _ => None,
        }
    }

    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Gone)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Planned,
                Self::Planned | Self::Creating | Self::Live | Self::Retiring | Self::Gone
            ) | (
                Self::Creating,
                Self::Creating | Self::Live | Self::Retiring | Self::Gone
            ) | (Self::Live, Self::Live | Self::Retiring | Self::Gone)
                | (Self::Retiring, Self::Retiring | Self::Gone)
                | (Self::Gone, Self::Gone)
        )
    }
}

/// Authoritative GitHub Actions job status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Unknown,
    Queued,
    InProgress,
    Completed,
}

impl JobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "queued" => Self::Queued,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Unknown,
        }
    }
}

/// Multi-dimensional compute capacity envelope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub vcpu: u32,
    pub instances: u32,
    pub disk_gb: u32,
}

impl Capacity {
    pub fn checked_add(self, other: Self) -> Result<Self, DomainError> {
        Ok(Self {
            vcpu: self
                .vcpu
                .checked_add(other.vcpu)
                .ok_or(DomainError::CapacityOverflow)?,
            instances: self
                .instances
                .checked_add(other.instances)
                .ok_or(DomainError::CapacityOverflow)?,
            disk_gb: self
                .disk_gb
                .checked_add(other.disk_gb)
                .ok_or(DomainError::CapacityOverflow)?,
        })
    }

    pub const fn fits_within(self, limit: Self) -> bool {
        self.vcpu <= limit.vcpu
            && self.instances <= limit.instances
            && self.disk_gb <= limit.disk_gb
    }
}

/// Ephemeral worker tracked by the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub name: String,
    pub pool_id: String,
    pub admission_id: Option<String>,
    pub request_id: String,
    pub provider: ProviderKind,
    pub placement_index: usize,
    pub region: String,
    pub zone: String,
    pub machine_type: String,
    pub provider_instance_id: Option<String>,
    pub phase: WorkerPhase,
    pub create_operation: Option<String>,
    pub delete_operation: Option<String>,
    pub delete_attempts: u32,
    pub bootstrap_used: bool,
    pub runner_id: Option<u64>,
    pub job_id: Option<u64>,
    pub retire_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// GitHub Actions workflow job observed by the control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub pool_id: String,
    #[serde(default)]
    pub repository: Option<String>,
    pub admission_id: Option<String>,
    pub run_id: Option<u64>,
    pub status: JobStatus,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
    pub checked_at: Option<u64>,
    pub updated_at: u64,
}

/// Network byte counters reported by an ephemeral worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUsage {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Machine-family backoff after a cloud quota, stockout, or rate-limit rejection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementBackoff {
    pub provider: ProviderKind,
    /// Backoff scope: a region (covers every zone in it) or a single zone.
    pub region: String,
    pub family: String,
    pub retry_after: u64,
    pub reason: String,
}

impl PlacementBackoff {
    /// Region-wide backoff for regional limits (quota, API rate limits).
    pub fn regional(placement: &Placement, retry_after: u64, reason: &str) -> Self {
        Self::scoped(placement, placement.region.clone(), retry_after, reason)
    }

    /// Zone-scoped backoff for stockouts so sibling zones in the region stay selectable.
    pub fn zonal(placement: &Placement, retry_after: u64, reason: &str) -> Self {
        Self::scoped(placement, placement.zone.clone(), retry_after, reason)
    }

    fn scoped(placement: &Placement, scope: String, retry_after: u64, reason: &str) -> Self {
        Self {
            provider: placement.provider,
            region: scope,
            family: placement.machine_family().to_owned(),
            retry_after,
            reason: reason.to_owned(),
        }
    }

    /// Returns `true` while this backoff blocks launches on `placement`.
    pub fn blocks(&self, placement: &Placement, now_unix: u64) -> bool {
        self.retry_after > now_unix
            && self.provider == placement.provider
            && (self.region == placement.region || self.region == placement.zone)
            && self.family == placement.machine_family()
    }
}

/// Authenticated workflow run requesting self-hosted capacity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequest {
    pub repository: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub head_sha: String,
    pub pools: BTreeSet<String>,
}

/// Reason a run was admitted or routed to fallback runners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    Admitted,
    Capacity,
    Unavailable,
    Rerun,
    NoWork,
    Deadline,
    DailyLimit,
    LifetimeLimit,
    ControlMode,
}

impl AdmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Capacity => "capacity",
            Self::Unavailable => "unavailable",
            Self::Rerun => "rerun",
            Self::NoWork => "no_work",
            Self::Deadline => "deadline",
            Self::DailyLimit => "daily_limit",
            Self::LifetimeLimit => "lifetime_limit",
            Self::ControlMode => "control_mode",
        }
    }
}

/// Status of a persisted admission reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Held,
    Released,
    Fallback,
}

impl AdmissionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Released => "released",
            Self::Fallback => "fallback",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "released" => Some(Self::Released),
            "fallback" => Some(Self::Fallback),
            _ => None,
        }
    }
}

/// Decision returned to the GitHub Actions routing step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionDecision {
    pub admitted: bool,
    pub target: String,
    pub reason: AdmissionReason,
    pub reservation: String,
    pub runners: BTreeMap<String, String>,
}

/// Durable record of a workflow run admission decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRecord {
    pub id: String,
    pub repository: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub head_sha: String,
    pub pools: BTreeSet<String>,
    pub target: String,
    pub reason: AdmissionReason,
    pub status: AdmissionStatus,
    pub capacity: Capacity,
    pub runners: BTreeMap<String, String>,
    pub created_at: u64,
    pub released_at: Option<u64>,
}

/// Structured audit entry for operator actions and safety events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub timestamp: u64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: serde_json::Value,
}

/// Real-time event broadcast to SSE subscribers and future UI clients.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    WorkerChanged {
        worker: String,
        pool_id: String,
        phase: WorkerPhase,
        timestamp: u64,
    },
    JobChanged {
        job_id: u64,
        pool_id: String,
        status: JobStatus,
        timestamp: u64,
    },
    AdmissionChanged {
        reservation: String,
        run_id: u64,
        status: AdmissionStatus,
        reason: AdmissionReason,
        timestamp: u64,
    },
    ControlModeChanged {
        mode: ControlMode,
        actor: String,
        reason: String,
        timestamp: u64,
    },
    BackoffTriggered {
        provider: ProviderKind,
        region: String,
        family: String,
        retry_after: u64,
    },
}
