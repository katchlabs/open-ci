//! Unified, vendor-neutral configuration schema and modular validators.

mod cache;
mod validate;

pub use cache::{
    AnyRepository, AwsCredentialsConfig, CacheAuthConfig, CacheRetentionConfig, CacheServiceConfig,
    CacheStorageConfig, DEFAULT_ACTIONS_RESULTS_URL, DEFAULT_ACTIONS_TOKEN_ISSUER,
    DEFAULT_GCS_ENDPOINT, GcpCredentialsConfig, RepositoryAllowlist,
};

use crate::error::ConfigError;
use crate::models::{AdmissionMode, Architecture, Capacity, GitHubScope, ProviderKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Top-level `open-ci` configuration loaded from `open-ci.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCiConfig {
    /// Single-repository target (`owner/repo`). Mutually exclusive with `organization`.
    #[serde(default)]
    pub repository: Option<String>,
    /// Organization-wide target (`org-name`). Mutually exclusive with `repository`.
    #[serde(default)]
    pub organization: Option<String>,
    /// Repository allowlist for org-level configs (e.g. `["my-org/*"]` or explicit `["my-org/api"]`).
    #[serde(default)]
    pub allowed_repositories: Vec<String>,
    pub server: ServerConfig,
    pub github: GitHubConfig,
    #[serde(default)]
    pub gcp: Option<GcpProviderConfig>,
    #[serde(default)]
    pub aws: Option<AwsProviderConfig>,
    #[serde(default)]
    pub cache: Option<CacheServiceConfig>,
    pub admission: AdmissionConfig,
    pub pools: BTreeMap<String, PoolConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub public_url: String,
    #[serde(default = "default_database_path")]
    pub database_path: String,
    #[serde(default = "default_tick_seconds")]
    pub tick_seconds: u64,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    #[serde(default)]
    pub allow_insecure_loopback: bool,
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    #[serde(default = "default_github_api_url")]
    pub api_url: String,
    #[serde(default = "default_oidc_issuer")]
    pub oidc_issuer: String,
    /// Optional runner group ID for org-level JIT runner registration (defaults to `1`).
    #[serde(default)]
    pub runner_group_id: Option<u64>,
    /// OIDC trust policy (required when `admission.mode` is `oidc` or `hybrid`).
    #[serde(default)]
    pub oidc_trust: Option<OidcTrustConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcTrustConfig {
    pub audience: String,
    /// Optional single-repository ID pin. Required for single-repo configs; omit for multi-repo orgs.
    #[serde(default)]
    pub repository_id: Option<u64>,
    pub owner_id: u64,
    pub subjects: BTreeSet<String>,
    /// Exact reusable workflow refs pinned to allowed 40-char commit SHAs.
    #[serde(default)]
    pub workflows: BTreeMap<String, BTreeSet<String>>,
    /// Standard repository workflow paths (e.g. `.github/workflows/ci.yml`) allowed on any commit.
    #[serde(default)]
    pub allowed_workflow_paths: BTreeSet<String>,
    #[serde(default)]
    pub caller_workflows: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcpProviderConfig {
    pub project: String,
    #[serde(default = "default_gcp_compute_url")]
    pub compute_url: String,
    pub image: String,
    #[serde(default)]
    pub external_ipv4: bool,
    #[serde(default = "default_network_tier")]
    pub network_tier: String,
    #[serde(default)]
    pub network_tags: Vec<String>,
    /// Optional target GCP service account email for IAM Credentials API impersonation (BYOC / cross-project).
    #[serde(default)]
    pub impersonate_service_account: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsProviderConfig {
    pub account_id: String,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// Optional IAM role ARN assumed via AWS STS `AssumeRole` (cross-account / BYOC).
    #[serde(default)]
    pub role_arn: Option<String>,
    /// Optional `ExternalId` passed to STS `AssumeRole` for confused-deputy protection.
    #[serde(default)]
    pub external_id: Option<String>,
    /// Optional STS role session name (defaults to `open-ci`).
    #[serde(default)]
    pub role_session_name: Option<String>,
    /// Keyless federation for controllers on GCP: exchange the host's GCE identity
    /// token (requested with this audience) for `role_arn` via STS
    /// `AssumeRoleWithWebIdentity`, instead of env keys or `IMDSv2`.
    #[serde(default)]
    pub web_identity_audience: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionConfig {
    #[serde(default)]
    pub mode: AdmissionMode,
    pub capacity: Capacity,
    #[serde(default)]
    pub background: Capacity,
    #[serde(default = "default_max_run_seconds")]
    pub max_run_seconds: u64,
    /// When true (default), workflow reruns (`run_attempt > 1`) automatically route to fallback runners.
    #[serde(default = "default_true")]
    pub fallback_on_rerun: bool,
    #[serde(default)]
    pub guardrails: Guardrails,
}

/// Optional, composable admission guardrails (workload- and vendor-neutral).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guardrails {
    #[serde(default)]
    pub max_concurrent_runs: Option<u32>,
    #[serde(default)]
    pub max_admitted_runs: Option<u32>,
    #[serde(default)]
    pub max_admitted_runs_per_utc_day: Option<u32>,
    #[serde(default)]
    pub admit_until_unix: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    pub runner_label: String,
    pub fallback_label: String,
    pub architecture: Architecture,
    pub vcpu: u32,
    pub disk_gb: u32,
    pub max_instances: usize,
    /// Optional per-run capacity reservation cap (defaults to `max_instances`).
    #[serde(default)]
    pub max_instances_per_run: Option<usize>,
    #[serde(default)]
    pub min_idle_instances: usize,
    #[serde(default)]
    pub max_total_instances: Option<usize>,
    #[serde(default = "default_creates_per_tick")]
    pub creates_per_tick: usize,
    #[serde(default = "default_lifetime_seconds")]
    pub lifetime_seconds: u64,
    #[serde(default = "default_idle_seconds")]
    pub idle_seconds: u64,
    #[serde(default = "default_job_poll_seconds")]
    pub job_poll_seconds: u64,
    #[serde(default = "default_cloud_discovery_seconds")]
    pub cloud_discovery_seconds: u64,
    #[serde(default)]
    pub prefer_cheapest: bool,
    #[serde(default)]
    pub provider_weights: BTreeMap<ProviderKind, u32>,
    /// Point this pool's jobs at `open-ci cache` by redirecting the runner's results URL.
    /// Jobs using `container:` or Docker container actions then lose `ACTIONS_RESULTS_URL`
    /// inside the container, so leave this off for pools that run them.
    #[serde(default)]
    pub transparent_cache: bool,
    pub placements: Vec<Placement>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub provider: ProviderKind,
    pub region: String,
    pub zone: String,
    pub machine_type: String,
    pub subnet: String,
    #[serde(default = "default_true")]
    pub spot: bool,
    #[serde(default)]
    pub disk_type: Option<String>,
    #[serde(default)]
    pub disk_interface: Option<String>,
    #[serde(default)]
    pub nic_type: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub security_group: Option<String>,
    #[serde(default)]
    pub root_device: Option<String>,
    #[serde(default)]
    pub max_spot_price_usd: Option<String>,
}

impl Placement {
    /// Extracts the machine family prefix used for regional quota backoff keys.
    pub fn machine_family(&self) -> &str {
        match self.provider {
            ProviderKind::Aws => self
                .machine_type
                .split('.')
                .next()
                .unwrap_or(&self.machine_type),
            ProviderKind::Gcp | ProviderKind::Mock => self
                .machine_type
                .split('-')
                .next()
                .unwrap_or(&self.machine_type),
        }
    }
}

impl PoolConfig {
    /// Worst-case number of runner instances reserved when a workflow run uses this pool.
    #[must_use]
    pub const fn reserved_instances_per_run(&self) -> usize {
        match self.max_instances_per_run {
            Some(cap) => cap,
            None => self.max_instances,
        }
    }

    /// Worst-case capacity envelope reserved when a workflow run uses this pool.
    pub fn capacity_footprint(&self) -> Result<Capacity, ConfigError> {
        let per_run = self.reserved_instances_per_run();
        let instances = u32::try_from(per_run)
            .map_err(|_| ConfigError::invalid("pools.max_instances_per_run", "exceeds u32"))?;
        let vcpu = self
            .vcpu
            .checked_mul(instances)
            .ok_or_else(|| ConfigError::invalid("pools.vcpu", "vcpu * max_instances overflows"))?;
        let disk_gb = self.disk_gb.checked_mul(instances).ok_or_else(|| {
            ConfigError::invalid("pools.disk_gb", "disk_gb * max_instances overflows")
        })?;
        Ok(Capacity {
            vcpu,
            instances,
            disk_gb,
        })
    }

    pub fn accepts_labels(&self, labels: &[String], scoped_label: Option<&str>) -> bool {
        let target = scoped_label.unwrap_or(&self.runner_label);
        let has_target = labels.iter().any(|l| l == target);
        let has_self_hosted = labels.iter().any(|l| l.eq_ignore_ascii_case("self-hosted"));
        has_target && (has_self_hosted || labels.len() == 1)
    }
}

impl OpenCiConfig {
    /// Cache URL handed to a pool's workers: only when `[cache]` exists and the pool opted in.
    pub fn worker_cache_url(&self, pool: &PoolConfig) -> Option<&str> {
        self.cache
            .as_ref()
            .filter(|_| pool.transparent_cache)
            .map(|cache| cache.public_url.as_str())
    }

    pub fn parse_toml(input: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(input)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate::validate_config(self)
    }

    /// Resolves the configured GitHub target scope (single repository or organization).
    pub fn github_scope(&self) -> GitHubScope {
        if let Some(org) = &self.organization {
            GitHubScope::Organization {
                org: org.clone(),
                allowed_repositories: self.allowed_repositories.clone(),
                runner_group_id: self.github.runner_group_id,
            }
        } else {
            GitHubScope::Repository(self.repository.clone().unwrap_or_default())
        }
    }
}

fn default_database_path() -> String {
    "open-ci.db".to_owned()
}
const fn default_tick_seconds() -> u64 {
    5
}
const fn default_request_timeout_seconds() -> u64 {
    20
}
fn default_github_api_url() -> String {
    "https://api.github.com".to_owned()
}
fn default_oidc_issuer() -> String {
    "https://token.actions.githubusercontent.com".to_owned()
}
fn default_gcp_compute_url() -> String {
    "https://compute.googleapis.com/compute/v1".to_owned()
}
fn default_network_tier() -> String {
    "PREMIUM".to_owned()
}
const fn default_max_run_seconds() -> u64 {
    3600
}
const fn default_creates_per_tick() -> usize {
    4
}
const fn default_lifetime_seconds() -> u64 {
    2400
}
const fn default_idle_seconds() -> u64 {
    120
}
const fn default_job_poll_seconds() -> u64 {
    15
}
const fn default_cloud_discovery_seconds() -> u64 {
    60
}
const fn default_true() -> bool {
    true
}
