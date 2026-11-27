//! Zero-cloud local demo configuration backed by `MockProvider`.

use open_ci_core::config::{
    AdmissionConfig, GitHubConfig, Guardrails, OidcTrustConfig, OpenCiConfig, Placement,
    PoolConfig, ServerConfig,
};
use open_ci_core::{AdmissionMode, Architecture, Capacity, ProviderKind};
use std::collections::{BTreeMap, BTreeSet};

fn demo_pool() -> PoolConfig {
    PoolConfig {
        runner_label: "open-ci-default".to_owned(),
        fallback_label: "ubuntu-latest".to_owned(),
        architecture: Architecture::X64,
        vcpu: 2,
        disk_gb: 30,
        max_instances: 4,
        max_instances_per_run: None,
        min_idle_instances: 0,
        max_total_instances: None,
        creates_per_tick: 2,
        lifetime_seconds: 1800,
        idle_seconds: 120,
        job_poll_seconds: 15,
        cloud_discovery_seconds: 60,
        prefer_cheapest: true,
        provider_weights: BTreeMap::new(),
        transparent_cache: false,
        placements: vec![Placement {
            provider: ProviderKind::Mock,
            region: "local-1".to_owned(),
            zone: "local-1a".to_owned(),
            machine_type: "mock-standard-2".to_owned(),
            subnet: "mock-subnet".to_owned(),
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
        }],
    }
}

pub fn build_demo_config(listen: &str) -> OpenCiConfig {
    let mut pools = BTreeMap::new();
    pools.insert("default".to_owned(), demo_pool());
    let mut workflows = BTreeMap::new();
    workflows.insert(
        "example/repo/.github/workflows/ci.yml@1111111111111111111111111111111111111111".to_owned(),
        BTreeSet::from(["1111111111111111111111111111111111111111".to_owned()]),
    );
    OpenCiConfig {
        repository: Some("example/repo".to_owned()),
        organization: None,
        allowed_repositories: Vec::new(),
        server: ServerConfig {
            listen: listen.to_owned(),
            public_url: format!("http://{listen}"),
            database_path: std::env::temp_dir()
                .join("open-ci-demo.db")
                .display()
                .to_string(),
            tick_seconds: 5,
            request_timeout_seconds: 10,
            allow_insecure_loopback: true,
            cors_allowed_origins: vec!["http://localhost:5173".to_owned()],
        },
        github: GitHubConfig {
            api_url: "http://127.0.0.1:9".to_owned(),
            oidc_issuer: "http://127.0.0.1:9".to_owned(),
            runner_group_id: None,
            oidc_trust: Some(OidcTrustConfig {
                audience: "open-ci:example/repo".to_owned(),
                repository_id: Some(1),
                owner_id: 1,
                subjects: BTreeSet::from(["repo:example/repo:pull_request".to_owned()]),
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
            mode: AdmissionMode::Hybrid,
            capacity: Capacity {
                vcpu: 16,
                instances: 8,
                disk_gb: 300,
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
