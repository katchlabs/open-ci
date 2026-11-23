//! End-to-end control-plane lifecycle tests using injected `ManualClock`,
//! fault-injecting `MockProvider`, and a real loopback fake GitHub server.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use open_ci_core::config::{
    AdmissionConfig, GitHubConfig, Guardrails, OidcTrustConfig, OpenCiConfig, Placement,
    PoolConfig, ServerConfig,
};
use open_ci_core::{
    AdmissionMode, AdmissionRequest, AdmissionStatus, Architecture, Capacity, Clock, ControlMode,
    ManualClock, ProviderKind, SecretString, WorkerPhase,
};
use open_ci_github::webhook::sign_webhook_body;
use open_ci_github::{GitHubClient, OidcVerifier, StaticGitHubToken};
use open_ci_provider::{CloudDisk, CloudInstance, InstanceStatus, ProviderError, ProviderRegistry};
use open_ci_provider_mock::MockProvider;
use open_ci_server::bootstrap::mint_bootstrap_token;
use open_ci_server::http::build_router;
use open_ci_server::{ControlPlane, ServerSecrets};
use open_ci_store::{SqliteStore, Store};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Default)]
struct FakeGitHubState {
    job_status: String,
    job_runner: Option<String>,
    runners: BTreeMap<u64, String>,
    deleted_runners: Vec<u64>,
    jit_calls: usize,
}

async fn start_fake_github(state: Arc<Mutex<FakeGitHubState>>) -> String {
    let s1 = Arc::clone(&state);
    let s2 = Arc::clone(&state);
    let s3 = Arc::clone(&state);
    let s4 = Arc::clone(&state);

    let app = Router::new()
        .route(
            "/repos/acme/app/actions/jobs/{id}",
            get(move |axum::extract::Path(id): axum::extract::Path<u64>| {
                let s = Arc::clone(&s1);
                async move {
                    let g = s.lock().await;
                    Json(json!({
                        "id": id,
                        "run_id": 77,
                        "status": g.job_status,
                        "conclusion": if g.job_status == "completed" { Some("success") } else { None },
                        "runner_name": g.job_runner,
                        "runner_id": g.job_runner.as_ref().map(|_| 9001u64),
                        "labels": ["self-hosted", "open-ci-api"],
                    }))
                }
            }),
        )
        .route(
            "/repos/acme/app/actions/runners",
            get(move || {
                let s = Arc::clone(&s2);
                async move {
                    let g = s.lock().await;
                    let list: Vec<_> = g
                        .runners
                        .iter()
                        .map(|(id, name)| json!({ "id": id, "name": name, "status": "online", "busy": false }))
                        .collect();
                    Json(json!({ "runners": list }))
                }
            }),
        )
        .route(
            "/repos/acme/app/actions/runners/generate-jitconfig",
            post(move |Json(body): Json<serde_json::Value>| {
                let s = Arc::clone(&s3);
                async move {
                    let mut g = s.lock().await;
                    g.jit_calls += 1;
                    let name = body["name"].as_str().unwrap_or("unknown").to_owned();
                    g.runners.insert(9001, name);
                    Json(json!({ "encoded_jit_config": "eyJqaXQiOiJ0ZXN0In0=" }))
                }
            }),
        )
        .route(
            "/repos/acme/app/actions/runners/{id}",
            delete(move |axum::extract::Path(id): axum::extract::Path<u64>| {
                let s = Arc::clone(&s4);
                async move {
                    let mut g = s.lock().await;
                    g.runners.remove(&id);
                    g.deleted_runners.push(id);
                    StatusCode::NO_CONTENT
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.ok();
    assert!(listener.is_some());
    if let Some(listener) = listener {
        let addr = listener.local_addr().ok();
        assert!(addr.is_some());
        if let Some(addr) = addr {
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            return format!("http://{addr}");
        }
    }
    String::new()
}

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
    }
}

fn test_config(gh_url: String) -> OpenCiConfig {
    let mut pools = BTreeMap::new();
    pools.insert(
        "api".to_owned(),
        PoolConfig {
            runner_label: "open-ci-api".to_owned(),
            fallback_label: "ubuntu-latest".to_owned(),
            architecture: Architecture::X64,
            vcpu: 2,
            disk_gb: 30,
            max_instances: 2,
            max_instances_per_run: None,
            min_idle_instances: 0,
            max_total_instances: Some(10),
            creates_per_tick: 2,
            lifetime_seconds: 1800,
            idle_seconds: 120,
            job_poll_seconds: 10,
            cloud_discovery_seconds: 60,
            prefer_cheapest: true,
            provider_weights: BTreeMap::new(),
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
            listen: "127.0.0.1:0".to_owned(),
            public_url: "https://ci.example.com".to_owned(),
            database_path: "open-ci.db".to_owned(),
            tick_seconds: 5,
            request_timeout_seconds: 10,
            allow_insecure_loopback: true,
            cors_allowed_origins: vec![],
        },
        github: GitHubConfig {
            api_url: gh_url.clone(),
            oidc_issuer: gh_url,
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

struct E2eHarness {
    _dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    store: SqliteStore,
    mock: MockProvider,
    secrets: ServerSecrets,
    cp: ControlPlane,
}

async fn setup_e2e_harness(gh_url: String) -> Option<E2eHarness> {
    let dir = tempdir().ok()?;
    let clock = Arc::new(ManualClock::new(10_000));
    let store = SqliteStore::open(&dir.path().join("open-ci.db"), 10_000).ok()?;
    let mock = MockProvider::new(ProviderKind::Mock);
    // Inject an orphan in placement 1 (us-east1-b) to prove placement index tracking works.
    mock.inject_orphan(
        CloudInstance {
            provider_id: "orphan-vm-1".to_owned(),
            name: "open-ci-api-orphan".to_owned(),
            pool_id: "api".to_owned(),
            status: InstanceStatus::Running,
            zone: "us-east1-b".to_owned(),
        },
        Some(CloudDisk {
            provider_id: "orphan-disk-1".to_owned(),
            name: "open-ci-api-orphan".to_owned(),
            pool_id: "api".to_owned(),
            attached: true,
            status: "READY".to_owned(),
        }),
    )
    .await;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(mock.clone()));

    let cfg = test_config(gh_url.clone());
    let scope = cfg.github_scope();
    let secrets = ServerSecrets {
        webhook_secret: SecretString::new("0123456789abcdef0123456789abcdef"),
        bootstrap_secret: SecretString::new("fedcba9876543210fedcba9876543210"),
        operator_token: Some(SecretString::new("operator-secret-token-12345678")),
        cache_signing_key: None,
    };
    let oidc = cfg.github.oidc_trust.clone().map(|trust| {
        OidcVerifier::new(gh_url.clone(), trust, scope.clone(), reqwest::Client::new())
    });
    let cp = ControlPlane::new(
        cfg,
        Arc::new(store.clone()),
        registry,
        GitHubClient::new(
            gh_url,
            scope,
            reqwest::Client::new(),
            Arc::new(StaticGitHubToken(SecretString::new("ghs_test"))),
        ),
        oidc,
        clock.clone(),
        secrets.clone(),
    )
    .ok()?;
    Some(E2eHarness {
        _dir: dir,
        clock,
        store,
        mock,
        secrets,
        cp,
    })
}

#[tokio::test]
async fn full_lifecycle_webhook_bootstrap_completion_and_orphan_cleanup() {
    let gh_state = Arc::new(Mutex::new(FakeGitHubState {
        job_status: "queued".to_owned(),
        ..FakeGitHubState::default()
    }));
    let gh_url = start_fake_github(Arc::clone(&gh_state)).await;
    let harness = setup_e2e_harness(gh_url).await;
    assert!(harness.is_some());
    let Some(E2eHarness {
        _dir,
        clock,
        store,
        mock,
        secrets,
        cp,
    }) = harness
    else {
        return;
    };

    let app = build_router(cp.clone());

    // 1. Deliver signed workflow_job webhook.
    let body = br#"{"action":"queued","repository":{"full_name":"acme/app"},"workflow_job":{"id":501,"run_id":77,"labels":["self-hosted","open-ci-api"]}}"#;
    let sig = sign_webhook_body(&secrets.webhook_secret, body).unwrap_or_default();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingress/webhook")
        .header("x-github-event", "workflow_job")
        .header("x-hub-signature-256", sig)
        .body(Body::from(&body[..]))
        .ok();
    assert!(req.is_some());
    if let Some(req) = req {
        let resp = app.clone().oneshot(req).await.ok();
        assert!(matches!(resp, Some(r) if r.status() == StatusCode::ACCEPTED));
    }

    // 2. First reconcile discovers the orphan in placement 1 AND creates a worker for job 501.
    let s1 = cp.reconcile_once().await.ok();
    assert!(matches!(s1, Some(s) if s.orphans_discovered == 1 && s.workers_created == 1));

    // Advance Creating -> Live (and retire orphan VM + disk).
    clock.advance(5);
    let _ = cp.reconcile_once().await;

    let workers = store.list_workers(Some("api")).await.unwrap_or_default();
    let live = workers
        .iter()
        .find(|w| w.name != "open-ci-api-orphan")
        .cloned();
    assert!(matches!(live.as_ref(), Some(w) if w.phase == WorkerPhase::Live));
    let Some(live_worker) = live else { return };

    // 3. One-time bootstrap succeeds once and fails on replay.
    let boot_tok =
        mint_bootstrap_token(&secrets.bootstrap_secret, &live_worker.name).unwrap_or_default();
    let jit1 = cp.handle_bootstrap(&live_worker.name, &boot_tok).await;
    assert!(jit1.is_ok());
    let jit2 = cp.handle_bootstrap(&live_worker.name, &boot_tok).await;
    assert!(jit2.is_err());

    // 4. Job runs on worker and completes -> reconciler deletes VM, disk, and GitHub runner.
    {
        let mut g = gh_state.lock().await;
        g.job_status = "completed".to_owned();
        g.job_runner = Some(live_worker.name.clone());
    }
    clock.advance(20);
    for _ in 0..5 {
        let _ = cp.reconcile_once().await;
        clock.advance(5);
    }

    // 5. Verify zero cloud instances, zero disks, and runner deleted from GitHub!
    assert_eq!(mock.counts().await, (0, 0));
    assert!(gh_state.lock().await.runners.is_empty());
    assert_eq!(gh_state.lock().await.deleted_runners, vec![9001]);

    // 6. Verify quota failure records regional backoff and routes next launch to placement 1.
    mock.inject_create_fault(ProviderError::QuotaExceeded {
        region: "us-west1".to_owned(),
        family: "n2d".to_owned(),
    })
    .await;
    let _ = cp.set_mode(ControlMode::Active, "test", "reset").await;
    gh_state.lock().await.job_status = "queued".to_owned();
    gh_state.lock().await.job_runner = None;
    let _ = store
        .observe_job_hint(
            502,
            "api",
            Some("acme/app"),
            None,
            Some(78),
            clock.now_unix(),
        )
        .await;
    let _ = cp.reconcile_once().await;

    let backoffs = store
        .list_active_backoffs(clock.now_unix())
        .await
        .unwrap_or_default();
    assert_eq!(backoffs.len(), 1);
    assert_eq!(backoffs[0].region, "us-west1");
}

struct FakeJobRecord {
    run_id: u64,
    status: String,
    runner: Option<String>,
    labels: Vec<String>,
}

#[derive(Default)]
struct OrgFakeGitHubState {
    jobs: BTreeMap<(String, u64), FakeJobRecord>,
    run_status: BTreeMap<(String, u64), String>,
    runners: BTreeMap<u64, String>,
    deleted_runners: Vec<u64>,
    jit_requests: Vec<(u64, String, Vec<String>)>,
    next_runner_id: u64,
}

async fn fake_get_job(
    state: Arc<Mutex<OrgFakeGitHubState>>,
    owner: String,
    repo: String,
    id: u64,
) -> (StatusCode, Json<serde_json::Value>) {
    let full = format!("{owner}/{repo}");
    let g = state.lock().await;
    let Some(job) = g.jobs.get(&(full, id)) else {
        return (StatusCode::NOT_FOUND, Json(json!({})));
    };
    let runner_id = job.runner.as_ref().and_then(|name| {
        g.runners
            .iter()
            .find(|(_, r)| *r == name)
            .map(|(rid, _)| *rid)
    });
    let conclusion = (job.status == "completed").then_some("success");
    (
        StatusCode::OK,
        Json(json!({
            "id": id,
            "run_id": job.run_id,
            "status": job.status,
            "conclusion": conclusion,
            "runner_name": job.runner,
            "runner_id": runner_id,
            "labels": job.labels,
        })),
    )
}

async fn fake_get_run(
    state: Arc<Mutex<OrgFakeGitHubState>>,
    owner: String,
    repo: String,
    run_id: u64,
) -> Json<serde_json::Value> {
    let full = format!("{owner}/{repo}");
    let g = state.lock().await;
    let status = g
        .run_status
        .get(&(full, run_id))
        .cloned()
        .unwrap_or_else(|| "in_progress".to_owned());
    let conclusion = (status == "completed").then_some("success");
    Json(json!({
        "id": run_id,
        "run_attempt": 1,
        "status": status,
        "conclusion": conclusion,
    }))
}

async fn fake_list_run_jobs(
    state: Arc<Mutex<OrgFakeGitHubState>>,
    owner: String,
    repo: String,
    run_id: u64,
) -> Json<serde_json::Value> {
    let full = format!("{owner}/{repo}");
    let g = state.lock().await;
    let list: Vec<_> = g
        .jobs
        .iter()
        .filter(|((r, _), job)| r == &full && job.run_id == run_id)
        .map(|((_, id), job)| {
            let conclusion = (job.status == "completed").then_some("success");
            json!({
                "id": id,
                "run_id": job.run_id,
                "status": job.status,
                "conclusion": conclusion,
                "runner_name": job.runner,
                "runner_id": None::<u64>,
                "labels": job.labels,
            })
        })
        .collect();
    Json(json!({ "jobs": list }))
}

async fn fake_generate_org_jit(
    state: Arc<Mutex<OrgFakeGitHubState>>,
    body: serde_json::Value,
) -> Json<serde_json::Value> {
    let mut g = state.lock().await;
    g.next_runner_id += 1;
    let rid = 8000 + g.next_runner_id;
    let group = body["runner_group_id"].as_u64().unwrap_or(0);
    let name = body["name"].as_str().unwrap_or("unknown").to_owned();
    let labels: Vec<String> = body["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    g.runners.insert(rid, name.clone());
    g.jit_requests.push((group, name, labels));
    Json(json!({ "encoded_jit_config": "org-jit-encoded" }))
}

async fn start_org_fake_github(state: Arc<Mutex<OrgFakeGitHubState>>) -> String {
    let s_job = Arc::clone(&state);
    let s_run = Arc::clone(&state);
    let s_run_jobs = Arc::clone(&state);
    let s_list = Arc::clone(&state);
    let s_jit = Arc::clone(&state);
    let s_del = Arc::clone(&state);

    let app = Router::new()
        .route(
            "/repos/{owner}/{repo}/actions/jobs/{id}",
            get(move |axum::extract::Path((o, r, id)): axum::extract::Path<(String, String, u64)>| {
                fake_get_job(Arc::clone(&s_job), o, r, id)
            }),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{run_id}",
            get(move |axum::extract::Path((o, r, rid)): axum::extract::Path<(String, String, u64)>| {
                fake_get_run(Arc::clone(&s_run), o, r, rid)
            }),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs",
            get(
                move |axum::extract::Path((o, r, rid, _a)): axum::extract::Path<(
                    String,
                    String,
                    u64,
                    u32,
                )>| fake_list_run_jobs(Arc::clone(&s_run_jobs), o, r, rid),
            ),
        )
        .route(
            "/orgs/acme/actions/runners",
            get(move || {
                let s = Arc::clone(&s_list);
                async move {
                    let g = s.lock().await;
                    let list: Vec<_> = g
                        .runners
                        .iter()
                        .map(|(id, name)| json!({ "id": id, "name": name, "status": "online", "busy": false }))
                        .collect();
                    Json(json!({ "runners": list }))
                }
            }),
        )
        .route(
            "/orgs/acme/actions/runners/generate-jitconfig",
            post(move |Json(body): Json<serde_json::Value>| {
                fake_generate_org_jit(Arc::clone(&s_jit), body)
            }),
        )
        .route(
            "/orgs/acme/actions/runners/{id}",
            delete(move |axum::extract::Path(id): axum::extract::Path<u64>| {
                let s = Arc::clone(&s_del);
                async move {
                    let mut g = s.lock().await;
                    g.runners.remove(&id);
                    g.deleted_runners.push(id);
                    StatusCode::NO_CONTENT
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.ok();
    assert!(listener.is_some());
    if let Some(listener) = listener {
        let addr = listener.local_addr().ok();
        assert!(addr.is_some());
        if let Some(addr) = addr {
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            return format!("http://{addr}");
        }
    }
    String::new()
}

fn setup_org_harness(gh_url: String, mode: AdmissionMode) -> Option<E2eHarness> {
    let dir = tempdir().ok()?;
    let clock = Arc::new(ManualClock::new(20_000));
    let store = SqliteStore::open(&dir.path().join("open-ci.db"), 20_000).ok()?;
    let mock = MockProvider::new(ProviderKind::Mock);
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(mock.clone()));

    let mut cfg = test_config(gh_url.clone());
    cfg.repository = None;
    cfg.organization = Some("acme".to_owned());
    cfg.allowed_repositories = vec!["acme/*".to_owned()];
    cfg.github.runner_group_id = Some(7);
    cfg.admission.mode = mode;
    cfg.admission.capacity = Capacity {
        vcpu: 32,
        instances: 16,
        disk_gb: 600,
    };
    cfg.github.oidc_trust = (mode != AdmissionMode::Webhook).then(|| OidcTrustConfig {
        audience: "open-ci:acme".to_owned(),
        repository_id: None,
        owner_id: 42,
        subjects: BTreeSet::from(["repo:acme/*:pull_request".to_owned()]),
        workflows: BTreeMap::new(),
        allowed_workflow_paths: BTreeSet::from([".github/workflows/ci.yml".to_owned()]),
        caller_workflows: BTreeSet::new(),
    });
    assert!(cfg.validate().is_ok());

    let scope = cfg.github_scope();
    let secrets = ServerSecrets {
        webhook_secret: SecretString::new("0123456789abcdef0123456789abcdef"),
        bootstrap_secret: SecretString::new("fedcba9876543210fedcba9876543210"),
        operator_token: Some(SecretString::new("operator-secret-token-12345678")),
        cache_signing_key: None,
    };
    let oidc = cfg.github.oidc_trust.clone().map(|trust| {
        OidcVerifier::new(gh_url.clone(), trust, scope.clone(), reqwest::Client::new())
    });
    let cp = ControlPlane::new(
        cfg,
        Arc::new(store.clone()),
        registry,
        GitHubClient::new(
            gh_url,
            scope,
            reqwest::Client::new(),
            Arc::new(StaticGitHubToken(SecretString::new("ghs_org_test"))),
        ),
        oidc,
        clock.clone(),
        secrets.clone(),
    )
    .ok()?;
    Some(E2eHarness {
        _dir: dir,
        clock,
        store,
        mock,
        secrets,
        cp,
    })
}

async fn send_signed_webhook(
    app: &Router,
    secret: &SecretString,
    payload: &[u8],
) -> Option<StatusCode> {
    let sig = sign_webhook_body(secret, payload).ok()?;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingress/webhook")
        .header("x-github-event", "workflow_job")
        .header("x-hub-signature-256", sig)
        .body(Body::from(payload.to_vec()))
        .ok()?;
    app.clone().oneshot(req).await.ok().map(|r| r.status())
}

#[tokio::test]
async fn org_webhook_mode_auto_admits_spins_up_org_runner_and_releases() {
    let gh_state = Arc::new(Mutex::new(OrgFakeGitHubState::default()));
    gh_state.lock().await.jobs.insert(
        ("acme/service-a".to_owned(), 701),
        FakeJobRecord {
            run_id: 88,
            status: "queued".to_owned(),
            runner: None,
            labels: vec!["self-hosted".to_owned(), "open-ci-api".to_owned()],
        },
    );
    let gh_url = start_org_fake_github(Arc::clone(&gh_state)).await;
    let harness = setup_org_harness(gh_url, AdmissionMode::Webhook);
    assert!(harness.is_some());
    let Some(E2eHarness {
        _dir,
        clock,
        store,
        mock,
        secrets,
        cp,
    }) = harness
    else {
        return;
    };
    let app = build_router(cp.clone());

    // Direct webhook for allowlisted org repo auto-admits without any prior /v1/ingress/admit call.
    let body = br#"{"action":"queued","repository":{"full_name":"acme/service-a"},"workflow_job":{"id":701,"run_id":88,"run_attempt":1,"labels":["self-hosted","open-ci-api"]}}"#;
    assert_eq!(
        send_signed_webhook(&app, &secrets.webhook_secret, body).await,
        Some(StatusCode::ACCEPTED)
    );

    let held = store.list_held_admissions().await.unwrap_or_default();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].repository, "acme/service-a");
    assert_eq!(
        held[0].runners.get("api").map(String::as_str),
        Some("open-ci-api")
    );

    // Reconcile creates and promotes worker to Live.
    let s1 = cp.reconcile_once().await.ok();
    assert!(matches!(s1, Some(s) if s.workers_created == 1));
    clock.advance(5);
    let _ = cp.reconcile_once().await;

    let workers = store.list_workers(Some("api")).await.unwrap_or_default();
    assert_eq!(workers.len(), 1);
    let worker_name = workers[0].name.clone();
    assert_eq!(workers[0].phase, WorkerPhase::Live);

    // Bootstrap uses org endpoint with runner_group_id=7 and unscoped label "open-ci-api".
    let boot_tok =
        mint_bootstrap_token(&secrets.bootstrap_secret, &worker_name).unwrap_or_default();
    assert!(cp.handle_bootstrap(&worker_name, &boot_tok).await.is_ok());
    {
        let g = gh_state.lock().await;
        assert_eq!(g.jit_requests.len(), 1);
        assert_eq!(g.jit_requests[0].0, 7);
        assert!(g.jit_requests[0].2.contains(&"open-ci-api".to_owned()));
    }

    // Complete job + workflow run -> worker and org runner cleaned up, admission released.
    {
        let mut g = gh_state.lock().await;
        g.jobs.insert(
            ("acme/service-a".to_owned(), 701),
            FakeJobRecord {
                run_id: 88,
                status: "completed".to_owned(),
                runner: Some(worker_name),
                labels: vec!["self-hosted".to_owned(), "open-ci-api".to_owned()],
            },
        );
        g.run_status
            .insert(("acme/service-a".to_owned(), 88), "completed".to_owned());
    }
    clock.advance(20);
    for _ in 0..5 {
        let _ = cp.reconcile_once().await;
        clock.advance(5);
    }

    assert_eq!(mock.counts().await, (0, 0));
    assert_eq!(gh_state.lock().await.deleted_runners, vec![8001]);
    assert!(
        store
            .list_held_admissions()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn webhook_mode_enforces_scope_paused_mode_and_disables_oidc_endpoint() {
    let gh_state = Arc::new(Mutex::new(OrgFakeGitHubState::default()));
    let gh_url = start_org_fake_github(Arc::clone(&gh_state)).await;
    let harness = setup_org_harness(gh_url, AdmissionMode::Webhook);
    assert!(harness.is_some());
    let Some(E2eHarness {
        _dir,
        store,
        secrets,
        cp,
        ..
    }) = harness
    else {
        return;
    };
    let app = build_router(cp.clone());

    // OIDC admission endpoint is disabled in webhook-only mode.
    let admit_req = Request::builder()
        .method("POST")
        .uri("/v1/ingress/admit")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pools":["api"]}"#))
        .ok();
    assert!(admit_req.is_some());
    if let Some(req) = admit_req {
        let resp = app.clone().oneshot(req).await.ok();
        assert!(matches!(resp, Some(r) if r.status() == StatusCode::FORBIDDEN));
    }

    // Foreign org webhook is rejected with 403.
    let foreign = br#"{"action":"queued","repository":{"full_name":"evil/service-a"},"workflow_job":{"id":700,"run_id":87,"labels":["self-hosted","open-ci-api"]}}"#;
    assert_eq!(
        send_signed_webhook(&app, &secrets.webhook_secret, foreign).await,
        Some(StatusCode::FORBIDDEN)
    );

    // Control mode Paused rejects direct webhook auto-admission without spawning workers.
    let _ = cp.set_mode(ControlMode::Paused, "ops", "maintenance").await;
    let paused_body = br#"{"action":"queued","repository":{"full_name":"acme/service-b"},"workflow_job":{"id":702,"run_id":89,"labels":["self-hosted","open-ci-api"]}}"#;
    assert_eq!(
        send_signed_webhook(&app, &secrets.webhook_secret, paused_body).await,
        Some(StatusCode::NO_CONTENT)
    );
    let adm = store
        .get_admission_by_run("acme/service-b", 89, 1)
        .await
        .ok()
        .flatten();
    assert!(matches!(adm, Some(a) if a.status == AdmissionStatus::Fallback));
    let s_paused = cp.reconcile_once().await.ok();
    assert!(matches!(s_paused, Some(s) if s.workers_created == 0));
}

#[tokio::test]
async fn hybrid_mode_handles_both_explicit_scoped_and_direct_webhook_admissions() {
    let gh_state = Arc::new(Mutex::new(OrgFakeGitHubState::default()));
    let gh_url = start_org_fake_github(Arc::clone(&gh_state)).await;
    let harness = setup_org_harness(gh_url, AdmissionMode::Hybrid);
    assert!(harness.is_some());
    let Some(E2eHarness {
        _dir,
        clock,
        store,
        secrets,
        cp,
        ..
    }) = harness
    else {
        return;
    };
    let app = build_router(cp.clone());

    // 1. Explicit admission (OIDC path) for acme/service-a gets a scoped runner label.
    let oidc_decision = cp
        .admit(&AdmissionRequest {
            repository: "acme/service-a".to_owned(),
            run_id: 301,
            run_attempt: 1,
            head_sha: "a".repeat(40),
            pools: BTreeSet::from(["api".to_owned()]),
        })
        .await
        .ok();
    assert!(matches!(oidc_decision, Some(ref d) if d.admitted));
    let Some(oidc_decision) = oidc_decision else {
        return;
    };
    let scoped_label = oidc_decision
        .runners
        .get("api")
        .cloned()
        .unwrap_or_default();
    assert!(scoped_label.starts_with("open-ci-api-open-ci-"));

    // Register jobs for both repos in fake GitHub.
    {
        let mut g = gh_state.lock().await;
        g.jobs.insert(
            ("acme/service-a".to_owned(), 801),
            FakeJobRecord {
                run_id: 301,
                status: "queued".to_owned(),
                runner: None,
                labels: vec!["self-hosted".to_owned(), scoped_label.clone()],
            },
        );
        g.jobs.insert(
            ("acme/service-b".to_owned(), 802),
            FakeJobRecord {
                run_id: 302,
                status: "queued".to_owned(),
                runner: None,
                labels: vec!["self-hosted".to_owned(), "open-ci-api".to_owned()],
            },
        );
    }

    // 2. Direct webhook for acme/service-b auto-admits with unscoped label.
    let direct_body = br#"{"action":"queued","repository":{"full_name":"acme/service-b"},"workflow_job":{"id":802,"run_id":302,"labels":["self-hosted","open-ci-api"]}}"#;
    assert_eq!(
        send_signed_webhook(&app, &secrets.webhook_secret, direct_body).await,
        Some(StatusCode::ACCEPTED)
    );

    // Reconcile spins up 2 workers (one per repo/admission).
    let s1 = cp.reconcile_once().await.ok();
    assert!(matches!(s1, Some(s) if s.workers_created == 2));
    clock.advance(5);
    let _ = cp.reconcile_once().await;

    let workers = store.list_workers(Some("api")).await.unwrap_or_default();
    assert_eq!(workers.len(), 2);
    for worker in &workers {
        let tok = mint_bootstrap_token(&secrets.bootstrap_secret, &worker.name).unwrap_or_default();
        assert!(cp.handle_bootstrap(&worker.name, &tok).await.is_ok());
    }

    let g = gh_state.lock().await;
    assert_eq!(g.jit_requests.len(), 2);
    let custom_labels: BTreeSet<String> = g
        .jit_requests
        .iter()
        .filter_map(|(_, _, labels)| labels.last().cloned())
        .collect();
    assert!(custom_labels.contains(&scoped_label));
    assert!(custom_labels.contains("open-ci-api"));
}
