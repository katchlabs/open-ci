//! Offline configuration validators for [`OpenCiConfig`].

use super::{
    AwsCredentialsConfig, AwsProviderConfig, CacheStorageConfig, GcpCredentialsConfig,
    OidcTrustConfig, OpenCiConfig, Placement, PoolConfig, RepositoryAllowlist,
};
use crate::error::ConfigError;
use crate::models::{GitHubScope, ProviderKind, is_valid_github_segment, split_repository};
use std::collections::BTreeSet;

pub(super) fn validate_config(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    validate_scope(cfg)?;
    validate_server(cfg)?;
    validate_github(cfg)?;
    validate_cache(cfg)?;
    validate_aws(cfg)?;
    validate_cloud_hypervisor(cfg)?;
    validate_admission(cfg)?;
    validate_pools(cfg)?;
    Ok(())
}

fn validate_scope(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    match (&cfg.repository, &cfg.organization) {
        (Some(repo), None) => {
            validate_repository(repo)?;
            if !cfg.allowed_repositories.is_empty() {
                return Err(ConfigError::invalid(
                    "allowed_repositories",
                    "only valid when `organization` is configured",
                ));
            }
        }
        (None, Some(org)) => {
            if !is_valid_github_segment(org) {
                return Err(ConfigError::invalid(
                    "organization",
                    "must be a valid GitHub organization name",
                ));
            }
            if cfg.allowed_repositories.is_empty() {
                return Err(ConfigError::invalid(
                    "allowed_repositories",
                    "at least one repository or `org/*` pattern is required when `organization` is set",
                ));
            }
            for pattern in &cfg.allowed_repositories {
                validate_repo_pattern("allowed_repositories", org, pattern)?;
            }
        }
        (Some(_), Some(_)) => {
            return Err(ConfigError::invalid(
                "repository",
                "specify either `repository` or `organization`, not both",
            ));
        }
        (None, None) => {
            return Err(ConfigError::invalid(
                "repository",
                "must specify either `repository` or `organization`",
            ));
        }
    }
    Ok(())
}

fn validate_server(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    let _addr: std::net::SocketAddr = cfg
        .server
        .listen
        .parse()
        .map_err(|_| ConfigError::invalid("server.listen", "must be host:port"))?;
    if !(1..=60).contains(&cfg.server.tick_seconds) {
        return Err(ConfigError::invalid(
            "server.tick_seconds",
            "must be within 1..=60",
        ));
    }
    if !(2..=120).contains(&cfg.server.request_timeout_seconds) {
        return Err(ConfigError::invalid(
            "server.request_timeout_seconds",
            "must be within 2..=120",
        ));
    }
    validate_url(
        "server.public_url",
        &cfg.server.public_url,
        cfg.server.allow_insecure_loopback,
    )?;
    for origin in &cfg.server.cors_allowed_origins {
        if origin.trim() == "*" {
            return Err(ConfigError::invalid(
                "server.cors_allowed_origins",
                "wildcard `*` origin is forbidden; specify explicit origins",
            ));
        }
        validate_url(
            "server.cors_allowed_origins",
            origin,
            cfg.server.allow_insecure_loopback,
        )?;
    }
    Ok(())
}

fn validate_github(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    validate_url(
        "github.api_url",
        &cfg.github.api_url,
        cfg.server.allow_insecure_loopback,
    )?;
    validate_url(
        "github.oidc_issuer",
        &cfg.github.oidc_issuer,
        cfg.server.allow_insecure_loopback,
    )?;
    if cfg.github.runner_group_id == Some(0) {
        return Err(ConfigError::invalid(
            "github.runner_group_id",
            "must be positive when set",
        ));
    }
    let scope = cfg.github_scope();
    match &cfg.github.oidc_trust {
        Some(trust) => validate_oidc_trust(trust, &scope)?,
        None if cfg.admission.mode.allows_oidc_admission() => {
            return Err(ConfigError::invalid(
                "github.oidc_trust",
                "required when admission.mode is `oidc` or `hybrid`",
            ));
        }
        None => {}
    }
    Ok(())
}

fn validate_oidc_trust(trust: &OidcTrustConfig, scope: &GitHubScope) -> Result<(), ConfigError> {
    if trust.audience.is_empty() || trust.audience.len() > 200 {
        return Err(ConfigError::invalid(
            "github.oidc_trust.audience",
            "must be 1..=200 chars",
        ));
    }
    if trust.owner_id == 0 {
        return Err(ConfigError::invalid(
            "github.oidc_trust.owner_id",
            "owner_id must be positive",
        ));
    }
    match (scope, trust.repository_id) {
        (GitHubScope::Repository(_), None | Some(0)) => {
            return Err(ConfigError::invalid(
                "github.oidc_trust.repository_id",
                "repository_id must be positive for single-repository configs",
            ));
        }
        (GitHubScope::Organization { .. }, Some(0)) => {
            return Err(ConfigError::invalid(
                "github.oidc_trust.repository_id",
                "repository_id must be positive when set",
            ));
        }
        _ => {}
    }
    if trust.subjects.is_empty() {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "at least one OIDC subject is required",
        ));
    }
    for subject in &trust.subjects {
        validate_oidc_subject(subject, scope)?;
    }
    if trust.workflows.is_empty() && trust.allowed_workflow_paths.is_empty() {
        return Err(ConfigError::invalid(
            "github.oidc_trust",
            "at least one pinned workflow SHA or allowed_workflow_paths entry is required",
        ));
    }
    for path in trust
        .caller_workflows
        .iter()
        .chain(trust.allowed_workflow_paths.iter())
    {
        validate_workflow_path("github.oidc_trust", path)?;
    }
    Ok(())
}

fn validate_oidc_subject(subject: &str, scope: &GitHubScope) -> Result<(), ConfigError> {
    if subject.is_empty() {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "subject must be non-empty",
        ));
    }
    if !subject.contains('*') {
        return Ok(());
    }
    let GitHubScope::Organization { org, .. } = scope else {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "wildcard subjects are only permitted for organization configs",
        ));
    };
    let Some(rest) = subject.strip_prefix("repo:") else {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "wildcard subject must start with `repo:`",
        ));
    };
    let Some((repo_pat, ctx)) = rest.split_once(':') else {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "wildcard subject must follow `repo:<org>/<pattern>:<context>`",
        ));
    };
    if ctx.is_empty() || ctx.contains('*') {
        return Err(ConfigError::invalid(
            "github.oidc_trust.subjects",
            "wildcard subject context must be non-empty and must not contain `*`",
        ));
    }
    validate_repo_pattern("github.oidc_trust.subjects", org, repo_pat)
}

fn validate_repo_pattern(
    field: &'static str,
    expected_org: &str,
    pattern: &str,
) -> Result<(), ConfigError> {
    let mut parts = pattern.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ConfigError::invalid(
            field,
            "pattern must be `<org>/<repo>`, `<org>/*`, or `<org>/<prefix>*`",
        ));
    };
    if owner != expected_org {
        return Err(ConfigError::invalid(
            field,
            "pattern owner must match configured `organization`",
        ));
    }
    if name == "*" {
        return Ok(());
    }
    if let Some(prefix) = name.strip_suffix('*') {
        if !prefix.is_empty() && !prefix.contains('*') && is_valid_github_segment(prefix) {
            return Ok(());
        }
        return Err(ConfigError::invalid(
            field,
            "wildcard prefix must be a valid non-empty repository prefix",
        ));
    }
    if is_valid_github_segment(name) {
        return Ok(());
    }
    Err(ConfigError::invalid(
        field,
        "repository pattern contains invalid characters",
    ))
}

fn validate_workflow_path(field: &'static str, path: &str) -> Result<(), ConfigError> {
    let Some(file) = path.strip_prefix(".github/workflows/") else {
        return Err(ConfigError::invalid(
            field,
            "workflow path must start with .github/workflows/",
        ));
    };
    let has_yaml_ext = std::path::Path::new(file)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"));
    if file.is_empty() || file.contains('/') || !has_yaml_ext {
        return Err(ConfigError::invalid(
            field,
            "workflow path must be an exact .yml or .yaml filename",
        ));
    }
    Ok(())
}

fn validate_cache(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    let Some(cache) = &cfg.cache else {
        return Ok(());
    };
    let _addr: std::net::SocketAddr = cache
        .listen
        .parse()
        .map_err(|_| ConfigError::invalid("cache.listen", "must be host:port"))?;
    require_http_url("cache.public_url", &cache.public_url)?;
    require_http_url("cache.upstream_results_url", &cache.upstream_results_url)?;
    require_http_url("cache.auth.issuer", &cache.auth.issuer)?;
    if let Some(jwks_url) = &cache.auth.jwks_url {
        require_http_url("cache.auth.jwks_url", jwks_url)?;
    }
    if cache
        .auth
        .audience
        .as_deref()
        .is_some_and(|aud| aud.trim().is_empty())
    {
        return Err(ConfigError::invalid(
            "cache.auth.audience",
            "must be non-empty when set",
        ));
    }
    if matches!(&cache.auth.repositories, RepositoryAllowlist::Ids(ids) if ids.is_empty()) {
        return Err(ConfigError::invalid(
            "cache.auth.repositories",
            "list at least one numeric repository ID, or set \"any\" to accept every repository",
        ));
    }
    if cache.max_entry_bytes == 0 || cache.signed_url_ttl_seconds == 0 {
        return Err(ConfigError::invalid(
            "cache",
            "max_entry_bytes and signed_url_ttl_seconds must be positive",
        ));
    }
    require_non_empty("cache.index_path", &cache.index_path)?;
    let retention = &cache.retention;
    if retention.max_total_bytes < cache.max_entry_bytes
        || retention.max_unused_days == 0
        || retention.sweep_interval_seconds == 0
    {
        return Err(ConfigError::invalid(
            "cache.retention",
            "max_total_bytes must be >= max_entry_bytes, and max_unused_days and sweep_interval_seconds must be positive",
        ));
    }
    validate_cache_storage(&cache.storage)
}

fn validate_cache_storage(storage: &CacheStorageConfig) -> Result<(), ConfigError> {
    match storage {
        CacheStorageConfig::Local { path } => require_non_empty("cache.storage.path", path),
        CacheStorageConfig::S3 {
            bucket,
            region,
            endpoint,
            prefix,
            credentials,
        } => {
            require_bucket("cache.storage.bucket", bucket)?;
            require_non_empty("cache.storage.region", region)?;
            if let Some(endpoint) = endpoint {
                require_http_url("cache.storage.endpoint", endpoint)?;
            }
            require_prefix(prefix)?;
            if let AwsCredentialsConfig::AssumeRole { role_arn, .. } = credentials
                && !role_arn.starts_with("arn:")
            {
                return Err(ConfigError::invalid(
                    "cache.storage.credentials.role_arn",
                    "must be an IAM role ARN",
                ));
            }
            Ok(())
        }
        CacheStorageConfig::Gcs {
            bucket,
            endpoint,
            prefix,
            credentials,
        } => {
            require_bucket("cache.storage.bucket", bucket)?;
            if let Some(endpoint) = endpoint {
                require_http_url("cache.storage.endpoint", endpoint)?;
            }
            require_prefix(prefix)?;
            match credentials {
                GcpCredentialsConfig::Metadata => Ok(()),
                GcpCredentialsConfig::Impersonate { service_account } => {
                    require_non_empty("cache.storage.credentials.service_account", service_account)
                }
                GcpCredentialsConfig::TokenFile { path } => {
                    require_non_empty("cache.storage.credentials.path", path)
                }
            }
        }
    }
}

fn require_http_url(field: &'static str, value: &str) -> Result<(), ConfigError> {
    let value = value.trim();
    let has_host = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));
    if has_host {
        Ok(())
    } else {
        Err(ConfigError::invalid(
            field,
            "must be an http:// or https:// URL with a host",
        ))
    }
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::invalid(field, "must be non-empty"))
    } else {
        Ok(())
    }
}

fn require_bucket(field: &'static str, bucket: &str) -> Result<(), ConfigError> {
    if bucket.trim().is_empty() || bucket.contains('/') {
        Err(ConfigError::invalid(
            field,
            "must be a bucket name without slashes",
        ))
    } else {
        Ok(())
    }
}

fn require_prefix(prefix: &str) -> Result<(), ConfigError> {
    if prefix.starts_with('/') || prefix.contains("..") {
        Err(ConfigError::invalid(
            "cache.storage.prefix",
            "must be relative and must not contain `..`",
        ))
    } else {
        Ok(())
    }
}

fn validate_admission(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    if !cfg.admission.background.fits_within(cfg.admission.capacity) {
        return Err(ConfigError::invalid(
            "admission.background",
            "background reserve exceeds total capacity",
        ));
    }
    if !(60..=21_600).contains(&cfg.admission.max_run_seconds) {
        return Err(ConfigError::invalid(
            "admission.max_run_seconds",
            "must be within 60..=21600",
        ));
    }
    let g = &cfg.admission.guardrails;
    if g.max_concurrent_runs == Some(0)
        || g.max_admitted_runs == Some(0)
        || g.max_admitted_runs_per_utc_day == Some(0)
        || g.admit_until_unix == Some(0)
    {
        return Err(ConfigError::invalid(
            "admission.guardrails",
            "optional limits must be positive when set",
        ));
    }
    Ok(())
}

fn validate_pools(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    if cfg.pools.is_empty() || cfg.pools.len() > 32 {
        return Err(ConfigError::invalid(
            "pools",
            "must define between 1 and 32 pools",
        ));
    }
    let mut seen_labels = BTreeSet::new();
    for (pool_id, pool) in &cfg.pools {
        validate_pool_id(pool_id)?;
        validate_runner_label("pools.runner_label", &pool.runner_label)?;
        validate_runner_label("pools.fallback_label", &pool.fallback_label)?;
        if pool.runner_label == pool.fallback_label {
            return Err(ConfigError::invalid(
                "pools.fallback_label",
                "fallback_label must differ from runner_label",
            ));
        }
        if !seen_labels.insert(&pool.runner_label) {
            return Err(ConfigError::invalid(
                "pools.runner_label",
                "runner_label must be unique across pools",
            ));
        }
        if pool.transparent_cache && cfg.cache.is_none() {
            return Err(ConfigError::invalid(
                "pools.transparent_cache",
                "requires a [cache] section",
            ));
        }
        validate_pool_bounds(cfg, pool)?;
        validate_pool_placements(cfg, pool)?;
    }
    Ok(())
}

fn validate_pool_bounds(cfg: &OpenCiConfig, pool: &PoolConfig) -> Result<(), ConfigError> {
    if pool.vcpu == 0 || pool.disk_gb < 10 || pool.max_instances == 0 {
        return Err(ConfigError::invalid(
            "pools",
            "vcpu > 0, disk_gb >= 10, and max_instances > 0 required",
        ));
    }
    if let Some(per_run) = pool.max_instances_per_run
        && (per_run == 0 || per_run > pool.max_instances)
    {
        return Err(ConfigError::invalid(
            "pools.max_instances_per_run",
            "must be within 1..=max_instances",
        ));
    }
    if pool.min_idle_instances > pool.max_instances {
        return Err(ConfigError::invalid(
            "pools.min_idle_instances",
            "cannot exceed max_instances",
        ));
    }
    if pool.creates_per_tick == 0 || pool.creates_per_tick > pool.max_instances {
        return Err(ConfigError::invalid(
            "pools.creates_per_tick",
            "must be within 1..=max_instances",
        ));
    }
    if !(120..=21_600).contains(&pool.lifetime_seconds) {
        return Err(ConfigError::invalid(
            "pools.lifetime_seconds",
            "must be within 120..=21600",
        ));
    }
    if pool.idle_seconds < 30 || pool.idle_seconds >= pool.lifetime_seconds {
        return Err(ConfigError::invalid(
            "pools.idle_seconds",
            "must be >= 30 and strictly less than lifetime_seconds",
        ));
    }
    let footprint = pool.capacity_footprint()?;
    let total = cfg
        .admission
        .background
        .checked_add(footprint)
        .map_err(|_| ConfigError::invalid("pools", "capacity footprint overflow"))?;
    if !total.fits_within(cfg.admission.capacity) {
        return Err(ConfigError::invalid(
            "pools",
            "single pool footprint plus background exceeds admission capacity",
        ));
    }
    Ok(())
}

fn validate_pool_placements(cfg: &OpenCiConfig, pool: &PoolConfig) -> Result<(), ConfigError> {
    if pool.placements.is_empty() || pool.placements.len() > 16 {
        return Err(ConfigError::invalid(
            "pools.placements",
            "must define 1..=16 placements",
        ));
    }
    let mut providers = BTreeSet::new();
    for placement in &pool.placements {
        providers.insert(placement.provider);
        validate_single_placement(cfg, placement)?;
    }
    for (provider, weight) in &pool.provider_weights {
        if *weight == 0 || !providers.contains(provider) {
            return Err(ConfigError::invalid(
                "pools.provider_weights",
                "weights must be positive and reference configured placement providers",
            ));
        }
    }
    Ok(())
}

fn validate_single_placement(cfg: &OpenCiConfig, placement: &Placement) -> Result<(), ConfigError> {
    if placement.region.is_empty()
        || placement.zone.is_empty()
        || placement.machine_type.is_empty()
        || placement.subnet.is_empty()
    {
        return Err(ConfigError::invalid(
            "pools.placements",
            "region, zone, machine_type, and subnet must be non-empty",
        ));
    }
    if !is_safe_cloud_token(&placement.region) || !is_safe_cloud_token(&placement.zone) {
        return Err(ConfigError::invalid(
            "pools.placements",
            "region and zone must contain only lowercase alphanumeric and hyphen chars",
        ));
    }
    if placement
        .threads_per_core
        .is_some_and(|t| !matches!(t, 1 | 2))
    {
        return Err(ConfigError::invalid(
            "pools.placements.threads_per_core",
            "must be 1 or 2",
        ));
    }
    if placement.core_count == Some(0) {
        return Err(ConfigError::invalid(
            "pools.placements.core_count",
            "must be greater than 0",
        ));
    }
    match placement.provider {
        ProviderKind::Gcp => validate_gcp_placement(cfg, placement),
        ProviderKind::Aws => validate_aws_placement(cfg, placement),
        ProviderKind::CloudHypervisor => validate_cloud_hypervisor_placement(cfg, placement),
        ProviderKind::Mock => Ok(()),
    }
}

fn validate_gcp_placement(cfg: &OpenCiConfig, placement: &Placement) -> Result<(), ConfigError> {
    let gcp = cfg
        .gcp
        .as_ref()
        .ok_or_else(|| ConfigError::invalid("gcp", "gcp config required for GCP placements"))?;
    if gcp.project.is_empty() || !is_safe_cloud_token(&gcp.project) {
        return Err(ConfigError::invalid(
            "gcp.project",
            "must be a non-empty valid GCP project ID",
        ));
    }
    let img = placement.image.as_deref().unwrap_or(&gcp.image);
    if !img.starts_with("projects/") || !img.contains("/global/images/") || img.contains("/family/")
    {
        return Err(ConfigError::invalid(
            "gcp.image",
            "must be a pinned GCP image resource (no /family/)",
        ));
    }
    if placement.core_count.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.core_count",
            "core_count is only supported on AWS placements",
        ));
    }
    if placement.ebs_iops.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_iops",
            "ebs_iops is only supported on AWS placements",
        ));
    }
    if placement.ebs_throughput_mbps.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_throughput_mbps",
            "ebs_throughput_mbps is only supported on AWS placements",
        ));
    }
    if placement.placement_group.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.placement_group",
            "placement_group is only supported on AWS placements",
        ));
    }
    if placement
        .local_ssd_count
        .is_some_and(|count| !(1..=24).contains(&count))
    {
        return Err(ConfigError::invalid(
            "pools.placements.local_ssd_count",
            "must be within 1..=24",
        ));
    }
    if placement
        .network_bandwidth_tier
        .as_deref()
        .is_some_and(|tier| !matches!(tier, "DEFAULT" | "TIER_1"))
    {
        return Err(ConfigError::invalid(
            "pools.placements.network_bandwidth_tier",
            "must be DEFAULT or TIER_1",
        ));
    }
    Ok(())
}

fn validate_aws_placement(cfg: &OpenCiConfig, placement: &Placement) -> Result<(), ConfigError> {
    if cfg.aws.is_none() {
        return Err(ConfigError::invalid(
            "aws",
            "aws config required for AWS placements",
        ));
    }
    if placement
        .image
        .as_deref()
        .is_none_or(|i| !i.starts_with("ami-"))
    {
        return Err(ConfigError::invalid(
            "pools.placements.image",
            "AWS placement requires a pinned ami-* image ID",
        ));
    }
    if placement
        .security_group
        .as_deref()
        .is_none_or(|sg| !sg.starts_with("sg-"))
    {
        return Err(ConfigError::invalid(
            "pools.placements.security_group",
            "AWS placement requires an sg-* security group ID",
        ));
    }
    if placement.core_count.is_some() != placement.threads_per_core.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.core_count",
            "AWS requires core_count and threads_per_core to be set together",
        ));
    }
    if placement.local_ssd_count.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.local_ssd_count",
            "local_ssd_count is only supported on GCP placements",
        ));
    }
    if placement.network_bandwidth_tier.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.network_bandwidth_tier",
            "network_bandwidth_tier is only supported on GCP placements",
        ));
    }
    if placement
        .ebs_iops
        .is_some_and(|iops| !(3_000..=64_000).contains(&iops))
    {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_iops",
            "must be within 3000..=64000",
        ));
    }
    if placement
        .ebs_throughput_mbps
        .is_some_and(|mbps| !(125..=1_000).contains(&mbps))
    {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_throughput_mbps",
            "must be within 125..=1000",
        ));
    }
    if placement.placement_group.as_deref().is_some_and(|group| {
        group.is_empty() || group.len() > 255 || !group.bytes().all(|b| b.is_ascii_graphic())
    }) {
        return Err(ConfigError::invalid(
            "pools.placements.placement_group",
            "must be 1..=255 non-whitespace printable ASCII characters",
        ));
    }
    Ok(())
}

fn validate_cloud_hypervisor_placement(
    cfg: &OpenCiConfig,
    placement: &Placement,
) -> Result<(), ConfigError> {
    if cfg.cloud_hypervisor.is_none() {
        return Err(ConfigError::invalid(
            "cloud_hypervisor",
            "cloud_hypervisor config required for Cloud Hypervisor placements",
        ));
    }
    if placement.core_count.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.core_count",
            "core_count is only supported on AWS placements",
        ));
    }
    if placement.local_ssd_count.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.local_ssd_count",
            "local_ssd_count is only supported on GCP placements",
        ));
    }
    if placement.network_bandwidth_tier.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.network_bandwidth_tier",
            "network_bandwidth_tier is only supported on GCP placements",
        ));
    }
    if placement.ebs_iops.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_iops",
            "ebs_iops is only supported on AWS placements",
        ));
    }
    if placement.ebs_throughput_mbps.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.ebs_throughput_mbps",
            "ebs_throughput_mbps is only supported on AWS placements",
        ));
    }
    if placement.placement_group.is_some() {
        return Err(ConfigError::invalid(
            "pools.placements.placement_group",
            "placement_group is only supported on AWS placements",
        ));
    }
    Ok(())
}

fn validate_cloud_hypervisor(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    let Some(ch) = &cfg.cloud_hypervisor else {
        return Ok(());
    };
    require_non_empty("cloud_hypervisor.binary", &ch.binary)?;
    require_non_empty("cloud_hypervisor.state_dir", &ch.state_dir)?;
    require_non_empty("cloud_hypervisor.kernel_path", &ch.kernel_path)?;
    require_non_empty("cloud_hypervisor.rootfs_image", &ch.rootfs_image)?;
    require_non_empty("cloud_hypervisor.bridge_name", &ch.bridge_name)?;
    if ch.bridge_name.len() > 15
        || !ch
            .bridge_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ConfigError::invalid(
            "cloud_hypervisor.bridge_name",
            "must be 1..=15 characters of [A-Za-z0-9._-]",
        ));
    }
    let valid_cidr = ch
        .subnet_cidr
        .trim()
        .split_once('/')
        .is_some_and(|(ip, p)| {
            ip.parse::<std::net::Ipv4Addr>().is_ok()
                && p.parse::<u32>().is_ok_and(|bits| (8..=30).contains(&bits))
        });
    if !valid_cidr {
        return Err(ConfigError::invalid(
            "cloud_hypervisor.subnet_cidr",
            "must be a valid IPv4 CIDR with prefix 8..=30 (e.g. `10.200.0.0/16`)",
        ));
    }
    if !(1..=16).contains(&ch.virtio_blk_queues) {
        return Err(ConfigError::invalid(
            "cloud_hypervisor.virtio_blk_queues",
            "must be within 1..=16",
        ));
    }
    if !(1..=128).contains(&ch.default_vcpus) {
        return Err(ConfigError::invalid(
            "cloud_hypervisor.default_vcpus",
            "must be within 1..=128",
        ));
    }
    if !(512..=524_288).contains(&ch.default_memory_mib) {
        return Err(ConfigError::invalid(
            "cloud_hypervisor.default_memory_mib",
            "must be within 512..=524288",
        ));
    }
    Ok(())
}

fn validate_aws(cfg: &OpenCiConfig) -> Result<(), ConfigError> {
    let Some(aws) = &cfg.aws else {
        return Ok(());
    };
    if aws.account_id.len() != 12 || !aws.account_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConfigError::invalid(
            "aws.account_id",
            "must be a 12-digit AWS account ID",
        ));
    }
    if aws
        .role_session_name
        .as_deref()
        .is_some_and(|name| !is_iam_name(name, 2, 64))
    {
        return Err(ConfigError::invalid(
            "aws.role_session_name",
            "must be 2-64 characters of [A-Za-z0-9+=,.@_-]",
        ));
    }
    match &aws.web_identity_audience {
        Some(audience) => validate_aws_web_identity(aws, audience),
        None => Ok(()),
    }
}

fn validate_aws_web_identity(aws: &AwsProviderConfig, audience: &str) -> Result<(), ConfigError> {
    if audience.is_empty()
        || audience.len() > 200
        || !audience.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(ConfigError::invalid(
            "aws.web_identity_audience",
            "must be 1-200 non-whitespace printable ASCII characters",
        ));
    }
    let role_in_account = aws.role_arn.as_deref().is_some_and(|arn| {
        arn.strip_prefix("arn:aws:iam::")
            .and_then(|rest| rest.strip_prefix(aws.account_id.as_str()))
            .and_then(|rest| rest.strip_prefix(":role/"))
            .is_some_and(is_iam_role_path)
    });
    if !role_in_account {
        return Err(ConfigError::invalid(
            "aws.role_arn",
            "web identity federation requires an IAM role ARN in aws.account_id",
        ));
    }
    if aws.external_id.is_some() {
        return Err(ConfigError::invalid(
            "aws.external_id",
            "AssumeRoleWithWebIdentity has no ExternalId; pin the audience in the role trust policy instead",
        ));
    }
    Ok(())
}

/// The part of a role ARN after `:role/`: optional printable path segments
/// followed by a role name.
fn is_iam_role_path(path: &str) -> bool {
    const MAX_PATH_AND_NAME: usize = 512 + 64;
    let mut segments = path.split('/');
    path.len() <= MAX_PATH_AND_NAME
        && segments
            .next_back()
            .is_some_and(|name| is_iam_name(name, 1, 64))
        && segments.all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_graphic()))
}

fn is_iam_name(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+=,.@_-".contains(&b))
}

fn is_safe_cloud_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn validate_repository(repo: &str) -> Result<(), ConfigError> {
    if split_repository(repo).is_none() {
        return Err(ConfigError::invalid(
            "repository",
            "must be `owner/name` with safe identifier characters",
        ));
    }
    Ok(())
}

fn validate_pool_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty()
        || id.len() > 24
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ConfigError::invalid(
            "pools",
            "pool IDs must be 1..=24 lowercase alphanumeric or hyphen chars",
        ));
    }
    Ok(())
}

fn validate_runner_label(field: &'static str, label: &str) -> Result<(), ConfigError> {
    if label.is_empty()
        || label.len() > 100
        || !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        || ["linux", "x64", "arm64", "self-hosted"]
            .iter()
            .any(|reserved| label.eq_ignore_ascii_case(reserved))
    {
        return Err(ConfigError::invalid(
            field,
            "must be a non-reserved runner label",
        ));
    }
    Ok(())
}

fn validate_url(
    field: &'static str,
    url: &str,
    allow_http_loopback: bool,
) -> Result<(), ConfigError> {
    if url.ends_with('/') || url.contains('?') || url.contains('#') || url.contains('@') {
        return Err(ConfigError::invalid(
            field,
            "URL must not contain credentials, query, fragment, or trailing slash",
        ));
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = extract_authority_host(rest);
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(ConfigError::invalid(field, "https URL must include a host"));
        }
        return Ok(());
    }
    if allow_http_loopback && let Some(rest) = url.strip_prefix("http://") {
        let host = extract_authority_host(rest);
        if matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return Ok(());
        }
    }
    Err(ConfigError::invalid(
        field,
        "must use https:// (or exact loopback http:// when allow_insecure_loopback = true)",
    ))
}

fn extract_authority_host(after_scheme: &str) -> &str {
    let authority = after_scheme.split('/').next().unwrap_or("");
    if let Some(bracket_end) = authority.find(']')
        && authority.starts_with('[')
    {
        return &authority[..=bracket_end];
    }
    authority.split(':').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_aws(extra: &str) -> Result<OpenCiConfig, ConfigError> {
        let toml = format!(
            r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[aws]
account_id = "123456789012"

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 8
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "aws"
region = "us-east-1"
zone = "us-east-1a"
machine_type = "c7i.4xlarge"
subnet = "subnet-0123456789abcdef0"
image = "ami-0123456789abcdef0"
security_group = "sg-0123456789abcdef0"
{extra}
"#
        );
        OpenCiConfig::parse_toml(&toml)
    }

    fn parse_gcp(extra: &str) -> Result<OpenCiConfig, ConfigError> {
        let toml = format!(
            r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[gcp]
project = "demo-proj"
image = "projects/demo-proj/global/images/runner-v1"

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 8
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "gcp"
region = "us-central1"
zone = "us-central1-a"
machine_type = "c3-standard-8"
subnet = "projects/demo-proj/regions/us-central1/subnetworks/ci"
{extra}
"#
        );
        OpenCiConfig::parse_toml(&toml)
    }

    fn invalid_field(res: &Result<OpenCiConfig, ConfigError>) -> Option<&'static str> {
        match res {
            Err(ConfigError::Invalid { field, .. }) => Some(field),
            _ => None,
        }
    }

    #[test]
    fn aws_placement_hardware_fields_validate() {
        assert!(parse_aws("").is_ok());
        assert!(
            parse_aws(
                r#"
core_count = 8
threads_per_core = 1
ebs_iops = 3000
ebs_throughput_mbps = 125
placement_group = "ci-cluster-01"
"#
            )
            .is_ok()
        );
        assert!(
            parse_aws(
                r"
core_count = 4
threads_per_core = 2
ebs_iops = 64000
ebs_throughput_mbps = 1000
"
            )
            .is_ok()
        );

        let long_group = format!("placement_group = \"{}\"", "a".repeat(256));
        let invalid_cases = [
            (
                "core_count = 0\nthreads_per_core = 1",
                "pools.placements.core_count",
            ),
            ("core_count = 8", "pools.placements.core_count"),
            ("threads_per_core = 1", "pools.placements.core_count"),
            (
                "core_count = 8\nthreads_per_core = 0",
                "pools.placements.threads_per_core",
            ),
            (
                "core_count = 8\nthreads_per_core = 3",
                "pools.placements.threads_per_core",
            ),
            ("local_ssd_count = 1", "pools.placements.local_ssd_count"),
            (
                "network_bandwidth_tier = \"TIER_1\"",
                "pools.placements.network_bandwidth_tier",
            ),
            ("ebs_iops = 2999", "pools.placements.ebs_iops"),
            ("ebs_iops = 64001", "pools.placements.ebs_iops"),
            (
                "ebs_throughput_mbps = 124",
                "pools.placements.ebs_throughput_mbps",
            ),
            (
                "ebs_throughput_mbps = 1001",
                "pools.placements.ebs_throughput_mbps",
            ),
            ("placement_group = \"\"", "pools.placements.placement_group"),
            (
                "placement_group = \"has space\"",
                "pools.placements.placement_group",
            ),
            (long_group.as_str(), "pools.placements.placement_group"),
        ];
        for (extra, expected_field) in invalid_cases {
            assert_eq!(
                invalid_field(&parse_aws(extra)),
                Some(expected_field),
                "aws case: {extra}"
            );
        }
    }

    #[test]
    fn gcp_placement_hardware_fields_validate() {
        assert!(parse_gcp("").is_ok());
        assert!(
            parse_gcp(
                r#"
threads_per_core = 1
local_ssd_count = 1
network_bandwidth_tier = "TIER_1"
"#
            )
            .is_ok()
        );
        assert!(
            parse_gcp(
                r#"
threads_per_core = 2
local_ssd_count = 24
network_bandwidth_tier = "DEFAULT"
"#
            )
            .is_ok()
        );

        let invalid_cases = [
            ("threads_per_core = 0", "pools.placements.threads_per_core"),
            ("threads_per_core = 3", "pools.placements.threads_per_core"),
            ("core_count = 4", "pools.placements.core_count"),
            ("ebs_iops = 3000", "pools.placements.ebs_iops"),
            (
                "ebs_throughput_mbps = 250",
                "pools.placements.ebs_throughput_mbps",
            ),
            (
                "placement_group = \"ci-pg\"",
                "pools.placements.placement_group",
            ),
            ("local_ssd_count = 0", "pools.placements.local_ssd_count"),
            ("local_ssd_count = 25", "pools.placements.local_ssd_count"),
            (
                "network_bandwidth_tier = \"ULTRA\"",
                "pools.placements.network_bandwidth_tier",
            ),
        ];
        for (extra, expected_field) in invalid_cases {
            assert_eq!(
                invalid_field(&parse_gcp(extra)),
                Some(expected_field),
                "gcp case: {extra}"
            );
        }
    }

    fn parse_ch(ch_extra: &str, placement_extra: &str) -> Result<OpenCiConfig, ConfigError> {
        let toml = format!(
            r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[cloud_hypervisor]
kernel_path = "/var/lib/open-ci/vmlinux"
rootfs_image = "/var/lib/open-ci/runner.raw"
{ch_extra}

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 4
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "cloud_hypervisor"
region = "local"
zone = "local-1"
machine_type = "ch-4c-16g"
subnet = "open-ci-br0"
{placement_extra}
"#
        );
        OpenCiConfig::parse_toml(&toml)
    }

    #[test]
    fn cloud_hypervisor_config_and_placements_validate() {
        assert!(parse_ch("", "").is_ok());
        assert!(parse_ch("", "threads_per_core = 1").is_ok());

        let invalid_ch_cases = [
            ("binary = \"\"", "cloud_hypervisor.binary"),
            ("state_dir = \"\"", "cloud_hypervisor.state_dir"),
            ("bridge_name = \"\"", "cloud_hypervisor.bridge_name"),
            (
                "subnet_cidr = \"10.200.0.0\"",
                "cloud_hypervisor.subnet_cidr",
            ),
            (
                "virtio_blk_queues = 0",
                "cloud_hypervisor.virtio_blk_queues",
            ),
            (
                "virtio_blk_queues = 17",
                "cloud_hypervisor.virtio_blk_queues",
            ),
            ("default_vcpus = 0", "cloud_hypervisor.default_vcpus"),
            ("default_vcpus = 129", "cloud_hypervisor.default_vcpus"),
            (
                "default_memory_mib = 256",
                "cloud_hypervisor.default_memory_mib",
            ),
            (
                "default_memory_mib = 524289",
                "cloud_hypervisor.default_memory_mib",
            ),
        ];
        for (extra, expected_field) in invalid_ch_cases {
            assert_eq!(
                invalid_field(&parse_ch(extra, "")),
                Some(expected_field),
                "cloud_hypervisor section case: {extra}"
            );
        }

        let invalid_placement_cases = [
            ("core_count = 4", "pools.placements.core_count"),
            ("local_ssd_count = 1", "pools.placements.local_ssd_count"),
            (
                "network_bandwidth_tier = \"TIER_1\"",
                "pools.placements.network_bandwidth_tier",
            ),
            ("ebs_iops = 3000", "pools.placements.ebs_iops"),
            (
                "ebs_throughput_mbps = 250",
                "pools.placements.ebs_throughput_mbps",
            ),
            (
                "placement_group = \"ci-pg\"",
                "pools.placements.placement_group",
            ),
        ];
        for (extra, expected_field) in invalid_placement_cases {
            assert_eq!(
                invalid_field(&parse_ch("", extra)),
                Some(expected_field),
                "cloud_hypervisor placement case: {extra}"
            );
        }

        let missing_section = OpenCiConfig::parse_toml(
            r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 4
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "cloud_hypervisor"
region = "local"
zone = "local-1"
machine_type = "ch-4c-16g"
subnet = "open-ci-br0"
"#,
        );
        assert_eq!(invalid_field(&missing_section), Some("cloud_hypervisor"));
    }
}
