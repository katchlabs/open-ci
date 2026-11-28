//! Property-based mathematical invariant proofs for `open-ci-core`.

use open_ci_core::ConfigError;
use open_ci_core::admission::{
    AdmissionContext, evaluate_admission, evaluate_webhook_admission, requested_capacity,
};
use open_ci_core::config::{
    AdmissionConfig, AwsProviderConfig, CacheServiceConfig, GitHubConfig, Guardrails,
    OidcTrustConfig, OpenCiConfig, Placement, PoolConfig, ServerConfig,
};
use open_ci_core::models::{
    AdmissionMode, AdmissionReason, AdmissionRecord, AdmissionRequest, AdmissionStatus,
    Architecture, Capacity, ControlMode, GitHubScope, Job, JobStatus, PlacementBackoff,
    ProviderKind, Worker, WorkerPhase,
};
use open_ci_core::reconcile::{
    LAUNCH_VISIBILITY_GRACE_SECONDS, RetireReason, evaluate_retirement, missing_instance_is_lost,
    plan_scale_up, select_placement, transition_phase,
};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

fn mock_placement(region: &str, zone: &str, machine_type: &str, subnet: &str) -> Placement {
    Placement {
        provider: ProviderKind::Mock,
        region: region.to_owned(),
        zone: zone.to_owned(),
        machine_type: machine_type.to_owned(),
        subnet: subnet.to_owned(),
        spot: true,
        disk_type: None,
        disk_interface: None,
        nic_type: None,
        image: None,
        security_group: None,
        root_device: None,
        max_spot_price_usd: None,
        core_count: None,
        threads_per_core: None,
        local_ssd_count: None,
        network_bandwidth_tier: None,
        ebs_iops: None,
        ebs_throughput_mbps: None,
        placement_group: None,
    }
}

fn sample_config(max_vcpu: u32, max_instances: u32, max_disk: u32) -> OpenCiConfig {
    let mut pools = BTreeMap::new();
    pools.insert(
        "api".to_owned(),
        PoolConfig {
            runner_label: "open-ci-api".to_owned(),
            fallback_label: "ubuntu-latest".to_owned(),
            architecture: Architecture::X64,
            vcpu: 2,
            disk_gb: 30,
            max_instances: 4,
            max_instances_per_run: None,
            min_idle_instances: 0,
            max_total_instances: Some(20),
            creates_per_tick: 2,
            lifetime_seconds: 1800,
            idle_seconds: 120,
            job_poll_seconds: 15,
            cloud_discovery_seconds: 60,
            prefer_cheapest: true,
            provider_weights: BTreeMap::new(),
            transparent_cache: false,
            placements: vec![
                mock_placement("us-west1", "us-west1-a", "n2d-standard-2", "subnet-a"),
                mock_placement("us-east1", "us-east1-b", "c2d-standard-2", "subnet-b"),
            ],
        },
    );

    let mut workflows = BTreeMap::new();
    workflows.insert(
        "acme/app/.github/workflows/ci.yml@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        BTreeSet::from(["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()]),
    );

    OpenCiConfig {
        repository: Some("acme/app".to_owned()),
        organization: None,
        allowed_repositories: Vec::new(),
        server: ServerConfig {
            listen: "127.0.0.1:8080".to_owned(),
            public_url: "https://ci.example.com".to_owned(),
            database_path: "open-ci.db".to_owned(),
            tick_seconds: 5,
            request_timeout_seconds: 20,
            allow_insecure_loopback: true,
            cors_allowed_origins: vec![],
        },
        github: GitHubConfig {
            api_url: "https://api.github.com".to_owned(),
            oidc_issuer: "https://token.actions.githubusercontent.com".to_owned(),
            runner_group_id: None,
            oidc_trust: Some(OidcTrustConfig {
                audience: "open-ci:acme/app".to_owned(),
                repository_id: Some(42),
                owner_id: 7,
                subjects: BTreeSet::from(["repo:acme/app:pull_request".to_owned()]),
                workflows,
                allowed_workflow_paths: BTreeSet::new(),
                caller_workflows: BTreeSet::new(),
            }),
        },
        gcp: None,
        aws: None,
        cloud_hypervisor: None,
        cache: None,
        admission: AdmissionConfig {
            mode: AdmissionMode::Oidc,
            capacity: Capacity {
                vcpu: max_vcpu,
                instances: max_instances,
                disk_gb: max_disk,
            },
            background: Capacity {
                vcpu: 2,
                instances: 1,
                disk_gb: 20,
            },
            max_run_seconds: 3600,
            fallback_on_rerun: true,
            guardrails: Guardrails::default(),
        },
        pools,
    }
}

proptest! {
    /// INVARIANT 1: Across arbitrary sequences of run admissions and releases,
    /// total held capacity + background reserve NEVER exceeds configured capacity.
    #[test]
    fn admission_never_exceeds_capacity_envelope(
        ops in prop::collection::vec(prop_oneof![Just(true), Just(false)], 1..100),
        max_vcpu in 10u32..64,
        max_instances in 5u32..32,
        max_disk in 140u32..1000,
    ) {
        let cfg = sample_config(max_vcpu, max_instances, max_disk);
        prop_assume!(cfg.validate().is_ok());

        let mut ctx = AdmissionContext {
            mode: ControlMode::Active,
            held_admissions: Vec::new(),
            total_admitted_count: 0,
            utc_day_admitted_count: 0,
            providers_healthy: true,
            pool_launched_counts: BTreeMap::new(),
        };

        for (step, is_admit) in ops.into_iter().enumerate() {
            if is_admit {
                let req = AdmissionRequest {
                    repository: "acme/app".to_owned(),
                    run_id: (step as u64) + 1,
                    run_attempt: 1,
                    head_sha: "a".repeat(40),
                    pools: BTreeSet::from(["api".to_owned()]),
                };
                let res_id = format!("res-{step}");
                let decision = evaluate_admission(&cfg, &ctx, &req, &res_id, 1_700_000_000)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;

                if decision.admitted {
                    let cap = requested_capacity(&cfg, &req.pools)
                        .map_err(|e| TestCaseError::fail(e.to_string()))?;
                    ctx.held_admissions.push(AdmissionRecord {
                        id: res_id,
                        repository: req.repository,
                        run_id: req.run_id,
                        run_attempt: req.run_attempt,
                        head_sha: req.head_sha,
                        pools: req.pools,
                        target: decision.target,
                        reason: decision.reason,
                        status: AdmissionStatus::Held,
                        capacity: cap,
                        runners: decision.runners,
                        created_at: 1_700_000_000,
                        released_at: None,
                    });
                    ctx.total_admitted_count += 1;
                    ctx.utc_day_admitted_count += 1;
                }
            } else if let Some(held) = ctx
                .held_admissions
                .iter_mut()
                .find(|r| r.status == AdmissionStatus::Held)
            {
                held.status = AdmissionStatus::Released;
                held.released_at = Some(1_700_000_100);
            }

            let mut used = cfg.admission.background;
            for r in ctx.held_admissions.iter().filter(|r| r.status == AdmissionStatus::Held) {
                used = used
                    .checked_add(r.capacity)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            prop_assert!(used.fits_within(cfg.admission.capacity));
        }
    }

    /// INVARIANT 2: Scaling never exceeds `max_instances`, `max_total_instances`, or `creates_per_tick`.
    #[test]
    fn scale_up_always_respects_all_caps(
        active in 0usize..20,
        launched in 0usize..40,
        queued in 0usize..50,
        unassigned in 0usize..20,
        draining in any::<bool>(),
    ) {
        let cfg = sample_config(32, 16, 500);
        let pool = cfg.pools.get("api").ok_or_else(|| TestCaseError::fail("missing pool"))?;
        let mode = if draining { ControlMode::Draining } else { ControlMode::Active };

        let to_create = plan_scale_up(pool, mode, active, launched, queued, unassigned);

        if draining {
            prop_assert_eq!(to_create, 0);
        }
        prop_assert!(to_create <= pool.creates_per_tick);
        prop_assert!(active.saturating_add(to_create) <= pool.max_instances.max(active));
        if let Some(total_cap) = pool.max_total_instances {
            prop_assert!(launched.saturating_add(to_create) <= total_cap.max(launched));
        }
    }

    /// INVARIANT 3: Every active worker MUST retire at or before `created_at + lifetime_seconds`,
    /// regardless of job state or control mode.
    #[test]
    fn worker_always_retires_at_hard_lifetime(
        created_at in 1_000u64..100_000,
        extra in 0u64..10_000,
        bootstrap_used in any::<bool>(),
        job_in_progress in any::<bool>(),
    ) {
        let cfg = sample_config(32, 16, 500);
        let pool = cfg.pools.get("api").ok_or_else(|| TestCaseError::fail("missing pool"))?;
        let worker = Worker {
            name: "open-ci-api-1".to_owned(),
            pool_id: "api".to_owned(),
            admission_id: None,
            request_id: "req-1".to_owned(),
            provider: ProviderKind::Mock,
            placement_index: 0,
            region: "us-west1".to_owned(),
            zone: "us-west1-a".to_owned(),
            machine_type: "n2d-standard-2".to_owned(),
            provider_instance_id: Some("mock-1".to_owned()),
            phase: WorkerPhase::Live,
            create_operation: None,
            delete_operation: None,
            delete_attempts: 0,
            bootstrap_used,
            runner_id: None,
            job_id: job_in_progress.then_some(99),
            retire_reason: None,
            created_at,
            updated_at: created_at,
        };
        let job = job_in_progress.then(|| Job {
            id: 99,
            pool_id: "api".to_owned(),
            repository: Some("acme/app".to_owned()),
            admission_id: None,
            run_id: Some(1),
            status: JobStatus::InProgress,
            conclusion: None,
            runner_name: Some(worker.name.clone()),
            runner_id: None,
            checked_at: Some(created_at),
            updated_at: created_at,
        });

        let expired_at = created_at.saturating_add(pool.lifetime_seconds).saturating_add(extra);
        let reason = evaluate_retirement(pool, &worker, job.as_ref(), ControlMode::Active, expired_at);
        prop_assert_eq!(reason, Some(RetireReason::LifetimeExpired));
    }

    /// INVARIANT 4: Placement selection never returns a backed-off placement.
    #[test]
    fn placement_selector_never_picks_backed_off_region(
        block_first in any::<bool>(),
        block_second in any::<bool>(),
        initial_cursor in 0u64..100,
    ) {
        let cfg = sample_config(32, 16, 500);
        let pool = cfg.pools.get("api").ok_or_else(|| TestCaseError::fail("missing pool"))?;
        let now = 5_000;
        let mut backoffs = Vec::new();
        if block_first {
            backoffs.push(PlacementBackoff {
                provider: ProviderKind::Mock,
                region: "us-west1".to_owned(),
                family: "n2d".to_owned(),
                retry_after: now + 300,
                reason: "quota".to_owned(),
            });
        }
        if block_second {
            backoffs.push(PlacementBackoff {
                provider: ProviderKind::Mock,
                region: "us-east1".to_owned(),
                family: "c2d".to_owned(),
                retry_after: now + 300,
                reason: "quota".to_owned(),
            });
        }

        let mut cursor = initial_cursor;
        let chosen = select_placement(pool, &backoffs, &mut cursor, now);
        match (block_first, block_second) {
            (true, true) => prop_assert_eq!(chosen, None),
            (true, false) => prop_assert_eq!(chosen, Some(1)),
            (false, true) => prop_assert_eq!(chosen, Some(0)),
            (false, false) => prop_assert!(chosen.is_some()),
        }
    }
}

proptest! {
    #[test]
    fn zonal_stockout_fails_over_to_sibling_zone_while_quota_benches_region(
        stocked_out in 0usize..3,
        now in 1_000u64..1_000_000,
    ) {
        let mut cfg = sample_config(32, 16, 500);
        let pool = cfg.pools.get_mut("api").ok_or_else(|| TestCaseError::fail("missing pool"))?;
        pool.placements = ["a", "b", "c"]
            .iter()
            .map(|z| mock_placement("us-central1", &format!("us-central1-{z}"), "c4a-standard-4-lssd", "s"))
            .collect();
        let pool = cfg.pools.get("api").ok_or_else(|| TestCaseError::fail("missing pool"))?;
        let stocked = pool.placements.get(stocked_out).ok_or_else(|| TestCaseError::fail("idx"))?;

        let zonal = vec![PlacementBackoff::zonal(stocked, now + 90, "capacity_unavailable")];
        let mut cursor = 0;
        let chosen = select_placement(pool, &zonal, &mut cursor, now);
        prop_assert!(chosen.is_some());
        prop_assert_ne!(chosen, Some(stocked_out));
        prop_assert_eq!(chosen, Some(usize::from(stocked_out == 0)));

        let regional = vec![PlacementBackoff::regional(stocked, now + 300, "quota_exceeded")];
        prop_assert_eq!(select_placement(pool, &regional, &mut cursor, now), None);

        let expired = vec![PlacementBackoff::zonal(stocked, now, "capacity_unavailable")];
        prop_assert_eq!(select_placement(pool, &expired, &mut cursor, now), Some(0));
    }
}

#[test]
fn reruns_always_route_to_fallback() {
    let cfg = sample_config(32, 16, 500);
    let ctx = AdmissionContext {
        mode: ControlMode::Active,
        providers_healthy: true,
        ..AdmissionContext::default()
    };
    let req = AdmissionRequest {
        repository: "acme/app".to_owned(),
        run_id: 100,
        run_attempt: 2,
        head_sha: "b".repeat(40),
        pools: BTreeSet::from(["api".to_owned()]),
    };
    let decision = evaluate_admission(&cfg, &ctx, &req, "res-1", 1000).ok();
    assert!(matches!(
        decision,
        Some(d) if !d.admitted && d.reason == AdmissionReason::Rerun
    ));
}

#[test]
fn worker_phase_cannot_move_backwards() {
    let mut worker = Worker {
        name: "open-ci-api-1".to_owned(),
        pool_id: "api".to_owned(),
        admission_id: None,
        request_id: "req-1".to_owned(),
        provider: ProviderKind::Mock,
        placement_index: 0,
        region: "us-west1".to_owned(),
        zone: "us-west1-a".to_owned(),
        machine_type: "n2d-standard-2".to_owned(),
        provider_instance_id: None,
        phase: WorkerPhase::Retiring,
        create_operation: None,
        delete_operation: None,
        delete_attempts: 0,
        bootstrap_used: true,
        runner_id: None,
        job_id: None,
        retire_reason: Some("job_completed".to_owned()),
        created_at: 100,
        updated_at: 100,
    };
    assert!(transition_phase(&mut worker, WorkerPhase::Live, 120).is_err());
    assert!(transition_phase(&mut worker, WorkerPhase::Gone, 120).is_ok());
    assert!(transition_phase(&mut worker, WorkerPhase::Retiring, 130).is_err());
}

#[test]
fn url_validation_rejects_loopback_lookalike_domains_and_wildcard_cors() {
    let mut cfg = sample_config(32, 16, 500);
    cfg.server.public_url = "http://127.0.0.1.evil.com".to_owned();
    assert!(cfg.validate().is_err());

    let mut cfg2 = sample_config(32, 16, 500);
    cfg2.server.cors_allowed_origins = vec!["*".to_owned()];
    assert!(cfg2.validate().is_err());
}

#[test]
fn reruns_are_admitted_when_fallback_on_rerun_is_disabled() {
    let mut cfg = sample_config(32, 16, 500);
    cfg.admission.fallback_on_rerun = false;
    let ctx = AdmissionContext {
        mode: ControlMode::Active,
        providers_healthy: true,
        ..AdmissionContext::default()
    };
    let req = AdmissionRequest {
        repository: "acme/app".to_owned(),
        run_id: 101,
        run_attempt: 2,
        head_sha: "c".repeat(40),
        pools: BTreeSet::from(["api".to_owned()]),
    };
    let decision = evaluate_admission(&cfg, &ctx, &req, "res-rerun", 1000).ok();
    assert!(matches!(
        decision,
        Some(d) if d.admitted && d.reason == AdmissionReason::Admitted
    ));
}

#[test]
fn pool_capacity_footprint_honors_max_instances_per_run() {
    let mut cfg = sample_config(32, 16, 500);
    if let Some(pool) = cfg.pools.get_mut("api") {
        pool.max_instances = 10;
        pool.max_instances_per_run = Some(2);
        let fp = pool.capacity_footprint().ok();
        assert_eq!(
            fp,
            Some(Capacity {
                vcpu: 4,
                instances: 2,
                disk_gb: 60,
            })
        );
    }
}

#[test]
fn github_scope_enforces_exact_prefix_wildcard_and_rejects_foreign_orgs() {
    let repo_scope = GitHubScope::Repository("acme/app".to_owned());
    assert!(repo_scope.allows_repository("acme/app"));
    assert!(!repo_scope.allows_repository("acme/other"));
    assert!(!repo_scope.allows_repository("evil/app"));
    assert_eq!(repo_scope.runner_group_id(), 1);

    let wildcard_org = GitHubScope::Organization {
        org: "acme".to_owned(),
        allowed_repositories: vec!["acme/*".to_owned()],
        runner_group_id: Some(7),
    };
    assert!(wildcard_org.allows_repository("acme/app"));
    assert!(wildcard_org.allows_repository("acme/service-billing"));
    assert!(!wildcard_org.allows_repository("other-org/app"));
    assert!(!wildcard_org.allows_repository("acme/.."));
    assert!(!wildcard_org.allows_repository("acme/a/b"));
    assert_eq!(wildcard_org.runner_group_id(), 7);

    let selective_org = GitHubScope::Organization {
        org: "acme".to_owned(),
        allowed_repositories: vec!["acme/api".to_owned(), "acme/svc-*".to_owned()],
        runner_group_id: None,
    };
    assert!(selective_org.allows_repository("acme/api"));
    assert!(selective_org.allows_repository("acme/svc-auth"));
    assert!(!selective_org.allows_repository("acme/web"));
    assert!(!selective_org.allows_repository("evil/svc-auth"));
    assert_eq!(selective_org.runner_group_id(), 1);
}

#[test]
fn org_level_config_validates_with_webhook_mode_and_optional_oidc_trust() {
    let mut cfg = sample_config(32, 16, 500);
    cfg.repository = None;
    cfg.organization = Some("acme".to_owned());
    cfg.allowed_repositories = vec!["acme/*".to_owned()];
    cfg.github.runner_group_id = Some(3);
    cfg.github.oidc_trust = None;
    cfg.admission.mode = AdmissionMode::Webhook;
    assert!(cfg.validate().is_ok());

    // OIDC mode requires oidc_trust.
    cfg.admission.mode = AdmissionMode::Oidc;
    assert!(cfg.validate().is_err());

    // Hybrid org config with owner_id + wildcard subject (no repository_id) is valid.
    cfg.admission.mode = AdmissionMode::Hybrid;
    cfg.github.oidc_trust = Some(OidcTrustConfig {
        audience: "open-ci:acme".to_owned(),
        repository_id: None,
        owner_id: 99,
        subjects: BTreeSet::from(["repo:acme/*:pull_request".to_owned()]),
        workflows: BTreeMap::new(),
        allowed_workflow_paths: BTreeSet::from([".github/workflows/ci.yml".to_owned()]),
        caller_workflows: BTreeSet::new(),
    });
    assert!(cfg.validate().is_ok());

    // Foreign org in allowed_repositories is rejected.
    cfg.allowed_repositories = vec!["evil/*".to_owned()];
    assert!(cfg.validate().is_err());

    // Both repository and organization set simultaneously is rejected.
    cfg.allowed_repositories = vec!["acme/*".to_owned()];
    cfg.repository = Some("acme/app".to_owned());
    assert!(cfg.validate().is_err());
}

#[test]
fn webhook_admission_returns_unscoped_labels_and_enforces_org_scope() {
    let mut cfg = sample_config(32, 16, 500);
    cfg.repository = None;
    cfg.organization = Some("acme".to_owned());
    cfg.allowed_repositories = vec!["acme/svc-*".to_owned()];
    cfg.admission.mode = AdmissionMode::Webhook;
    cfg.github.oidc_trust = None;

    let ctx = AdmissionContext {
        mode: ControlMode::Active,
        providers_healthy: true,
        ..AdmissionContext::default()
    };

    let ok_req = AdmissionRequest {
        repository: "acme/svc-payments".to_owned(),
        run_id: 200,
        run_attempt: 1,
        head_sha: String::new(),
        pools: BTreeSet::from(["api".to_owned()]),
    };
    let decision = evaluate_webhook_admission(&cfg, &ctx, &ok_req, "res-wh-1", 1000).ok();
    assert!(matches!(
        decision,
        Some(ref d) if d.admitted && d.runners.get("api").map(String::as_str) == Some("open-ci-api")
    ));

    let foreign_req = AdmissionRequest {
        repository: "other-org/svc-payments".to_owned(),
        ..ok_req
    };
    assert!(evaluate_webhook_admission(&cfg, &ctx, &foreign_req, "res-wh-2", 1000).is_err());
}

#[test]
fn example_toml_parses_and_validates() {
    let raw = include_str!("../../../config/open-ci.example.toml");
    let parsed = OpenCiConfig::parse_toml(raw);
    assert!(parsed.is_ok(), "example config failed: {parsed:?}");
}

#[test]
fn workers_get_the_cache_url_only_when_their_pool_opts_in() -> Result<(), Box<dyn std::error::Error>>
{
    let mut cfg = sample_config(64, 16, 1_000);
    let pool_id = cfg.pools.keys().next().cloned().ok_or("pool")?;
    let pool = |cfg: &OpenCiConfig| cfg.pools.get(&pool_id).cloned().ok_or("pool");

    assert_eq!(
        cfg.worker_cache_url(&pool(&cfg)?),
        None,
        "no [cache] section"
    );

    cfg.cache = Some(toml::from_str::<CacheServiceConfig>(
        "public_url = \"http://10.0.0.5:8090\"\n[auth]\nrepositories = [1]\n",
    )?);
    assert_eq!(
        cfg.worker_cache_url(&pool(&cfg)?),
        None,
        "pool did not opt in"
    );

    if let Some(p) = cfg.pools.get_mut(&pool_id) {
        p.transparent_cache = true;
    }
    assert_eq!(
        cfg.worker_cache_url(&pool(&cfg)?),
        Some("http://10.0.0.5:8090")
    );
    assert!(cfg.validate().is_ok());

    cfg.cache = None;
    assert!(
        cfg.validate().is_err(),
        "transparent_cache requires a [cache] section"
    );
    Ok(())
}

fn federated_aws() -> AwsProviderConfig {
    AwsProviderConfig {
        account_id: "123456789012".to_owned(),
        endpoint_url: None,
        role_arn: Some("arn:aws:iam::123456789012:role/open-ci-gcp-controller".to_owned()),
        external_id: None,
        role_session_name: None,
        web_identity_audience: Some("open-ci-aws-federation".to_owned()),
    }
}

fn invalid_aws_field(aws: AwsProviderConfig) -> Option<&'static str> {
    let mut cfg = sample_config(32, 16, 500);
    cfg.aws = Some(aws);
    match cfg.validate() {
        Err(ConfigError::Invalid { field, .. }) => Some(field),
        _ => None,
    }
}

#[test]
fn aws_web_identity_accepts_a_same_account_role() {
    assert_eq!(invalid_aws_field(federated_aws()), None);
    let nested_path = AwsProviderConfig {
        role_arn: Some("arn:aws:iam::123456789012:role/team/ci/open-ci".to_owned()),
        ..federated_aws()
    };
    assert_eq!(invalid_aws_field(nested_path), None);
}

#[test]
fn aws_web_identity_rejects_each_invalid_setting_without_aws_placements() {
    let cases = [
        ("", "aws.web_identity_audience"),
        ("has space", "aws.web_identity_audience"),
        (&"a".repeat(201), "aws.web_identity_audience"),
    ];
    for (audience, field) in cases {
        let aws = AwsProviderConfig {
            web_identity_audience: Some(audience.to_owned()),
            ..federated_aws()
        };
        assert_eq!(invalid_aws_field(aws), Some(field), "audience {audience:?}");
    }
    for role_arn in [
        None,
        Some("arn:aws:iam::999999999999:role/open-ci-gcp-controller"),
        Some("arn:aws:iam::123456789012:role/"),
        Some("arn:aws:iam::123456789012:user/open-ci"),
        Some("arn:aws:iam::123456789012:role//open-ci"),
        Some("arn:aws:iam::123456789012:role/bad path/open-ci"),
        Some("arn:aws:iam::123456789012:role/a\nb/open-ci"),
    ] {
        let aws = AwsProviderConfig {
            role_arn: role_arn.map(str::to_owned),
            ..federated_aws()
        };
        assert_eq!(invalid_aws_field(aws), Some("aws.role_arn"), "{role_arn:?}");
    }
    let with_external_id = AwsProviderConfig {
        external_id: Some("tenant".to_owned()),
        ..federated_aws()
    };
    assert_eq!(invalid_aws_field(with_external_id), Some("aws.external_id"));
    let bad_session = AwsProviderConfig {
        role_session_name: Some("x".to_owned()),
        ..federated_aws()
    };
    assert_eq!(
        invalid_aws_field(bad_session),
        Some("aws.role_session_name")
    );
}

fn live_worker(created_at: u64, bootstrap_used: bool) -> Worker {
    Worker {
        name: "open-ci-api-1".to_owned(),
        pool_id: "api".to_owned(),
        admission_id: None,
        request_id: "req-1".to_owned(),
        provider: ProviderKind::Aws,
        placement_index: 0,
        region: "us-east-1".to_owned(),
        zone: "us-east-1a".to_owned(),
        machine_type: "c7g.xlarge".to_owned(),
        provider_instance_id: Some("i-0abc".to_owned()),
        phase: WorkerPhase::Live,
        create_operation: None,
        delete_operation: None,
        delete_attempts: 0,
        bootstrap_used,
        runner_id: None,
        job_id: None,
        retire_reason: None,
        created_at,
        updated_at: created_at,
    }
}

#[test]
fn unlisted_launch_is_lost_only_after_visibility_grace() {
    let created = 10_000;
    let launching = live_worker(created, false);
    let grace_ends = created + LAUNCH_VISIBILITY_GRACE_SECONDS;
    assert!(!missing_instance_is_lost(&launching, created - 5));
    assert!(!missing_instance_is_lost(&launching, created));
    assert!(!missing_instance_is_lost(&launching, grace_ends - 1));
    assert!(missing_instance_is_lost(&launching, grace_ends));
}

#[test]
fn unlisted_bootstrapped_worker_is_lost_immediately() {
    let created = 10_000;
    assert!(missing_instance_is_lost(
        &live_worker(created, true),
        created
    ));
}

#[test]
fn admission_fails_closed_when_pool_max_total_instances_cannot_cover_run() {
    let cfg = sample_config(64, 16, 1_000);
    let req = AdmissionRequest {
        repository: "acme/app".to_owned(),
        run_id: 42,
        run_attempt: 1,
        head_sha: "a".repeat(40),
        pools: BTreeSet::from(["api".to_owned()]),
    };

    let mut ctx_ok = AdmissionContext {
        mode: ControlMode::Active,
        providers_healthy: true,
        pool_launched_counts: BTreeMap::from([("api".to_owned(), 16)]),
        ..AdmissionContext::default()
    };
    let ok = evaluate_admission(&cfg, &ctx_ok, &req, "res-ok", 1_000).ok();
    assert!(matches!(ok, Some(ref d) if d.admitted && d.reason == AdmissionReason::Admitted));

    ctx_ok.pool_launched_counts.insert("api".to_owned(), 17);
    let rejected = evaluate_admission(&cfg, &ctx_ok, &req, "res-cap", 1_000).ok();
    assert!(matches!(
        rejected,
        Some(ref d)
            if !d.admitted
                && d.target == "fallback"
                && d.reason == AdmissionReason::LifetimeLimit
                && d.runners.get("api").map(String::as_str) == Some("ubuntu-latest")
    ));
}
