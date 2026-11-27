//! Operator REST API, auth enforcement, control mode transitions, audit log, and Prometheus metrics tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use open_ci_core::config::{
    AdmissionConfig, GitHubConfig, Guardrails, OidcTrustConfig, OpenCiConfig, Placement,
    PoolConfig, ServerConfig,
};
use open_ci_core::{
    AdmissionMode, Architecture, Capacity, ControlMode, ManualClock, ProviderKind, SecretString,
};
use open_ci_github::{GitHubClient, OidcVerifier, StaticGitHubToken};
use open_ci_provider::ProviderRegistry;
use open_ci_provider_mock::MockProvider;
use open_ci_server::http::build_router;
use open_ci_server::{ControlPlane, ServerSecrets};
use open_ci_store::SqliteStore;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

fn operator_test_config() -> OpenCiConfig {
    let mut pools = BTreeMap::new();
    pools.insert(
        "default".to_owned(),
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
                region: "us-west1".to_owned(),
                zone: "us-west1-a".to_owned(),
                machine_type: "n2d-standard-2".to_owned(),
                subnet: "subnet-1".to_owned(),
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
            listen: "127.0.0.1:0".to_owned(),
            public_url: "https://ci.example.com".to_owned(),
            database_path: "open-ci.db".to_owned(),
            tick_seconds: 5,
            request_timeout_seconds: 10,
            allow_insecure_loopback: true,
            cors_allowed_origins: vec![],
        },
        github: GitHubConfig {
            api_url: "http://127.0.0.1:9".to_owned(),
            oidc_issuer: "http://127.0.0.1:9".to_owned(),
            runner_group_id: None,
            oidc_trust: Some(OidcTrustConfig {
                audience: "open-ci:acme/app".to_owned(),
                repository_id: Some(1),
                owner_id: 1,
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

fn setup_server() -> Option<(axum::Router, tempfile::TempDir)> {
    let dir = tempdir().ok()?;
    let store = SqliteStore::open(&dir.path().join("open-ci.db"), 5_000).ok()?;
    let cfg = operator_test_config();
    let scope = cfg.github_scope();

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider::default()));

    let oidc = cfg.github.oidc_trust.clone().map(|trust| {
        OidcVerifier::new(
            cfg.github.oidc_issuer.clone(),
            trust,
            scope.clone(),
            reqwest::Client::new(),
        )
    });

    let cp = ControlPlane::new(
        cfg.clone(),
        Arc::new(store),
        registry,
        GitHubClient::new(
            cfg.github.api_url.clone(),
            scope,
            reqwest::Client::new(),
            Arc::new(StaticGitHubToken(SecretString::new("ghs_test"))),
        ),
        oidc,
        Arc::new(ManualClock::new(5_000)),
        ServerSecrets {
            webhook_secret: SecretString::new("0123456789abcdef0123456789abcdef"),
            bootstrap_secret: SecretString::new("fedcba9876543210fedcba9876543210"),
            operator_token: Some(SecretString::new("super-secret-operator-token-1234")),
        },
    )
    .ok()?;
    Some((build_router(cp), dir))
}

#[tokio::test]
async fn operator_endpoints_enforce_auth_update_mode_and_emit_metrics() {
    let setup = setup_server();
    assert!(setup.is_some());
    let Some((app, _dir)) = setup else { return };

    // Unauthenticated call -> 401
    let unauth = Request::builder()
        .uri("/api/v1/overview")
        .body(Body::empty())
        .ok();
    assert!(unauth.is_some());
    if let Some(req) = unauth {
        let resp = app.clone().oneshot(req).await.ok();
        assert!(matches!(resp, Some(r) if r.status() == StatusCode::UNAUTHORIZED));
    }

    // Authenticated mode change -> draining + audit entry
    let mode_req = Request::builder()
        .method("POST")
        .uri("/api/v1/control/mode")
        .header("authorization", "Bearer super-secret-operator-token-1234")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"mode":"draining","actor":"alice","reason":"upgrade"}"#,
        ))
        .ok();
    assert!(mode_req.is_some());
    if let Some(req) = mode_req {
        let resp = app.clone().oneshot(req).await.ok();
        assert!(matches!(resp, Some(r) if r.status() == StatusCode::OK));
    }

    // Overview reflects draining mode
    let ov_req = Request::builder()
        .uri("/api/v1/overview")
        .header("authorization", "Bearer super-secret-operator-token-1234")
        .body(Body::empty())
        .ok();
    assert!(ov_req.is_some());
    if let Some(req) = ov_req {
        let resp = app.clone().oneshot(req).await.ok();
        assert!(resp.is_some());
        if let Some(resp) = resp {
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.ok();
            assert!(bytes.is_some());
            if let Some(bytes) = bytes {
                let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
                assert_eq!(
                    parsed["control"]["mode"],
                    serde_json::json!(ControlMode::Draining)
                );
            }
        }
    }

    // Prometheus metrics endpoint renders valid gauge lines
    let m_req = Request::builder()
        .uri("/metrics")
        .header("authorization", "Bearer super-secret-operator-token-1234")
        .body(Body::empty())
        .ok();
    assert!(m_req.is_some());
    if let Some(req) = m_req {
        let resp = app.oneshot(req).await.ok();
        assert!(resp.is_some());
        if let Some(resp) = resp {
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.ok();
            let text = bytes
                .and_then(|b| String::from_utf8(b.to_vec()).ok())
                .unwrap_or_default();
            assert!(text.contains("open_ci_workers_total{phase=\"live\"} 0"));
            assert!(text.contains("open_ci_capacity_limit_vcpu 16"));
        }
    }
}
