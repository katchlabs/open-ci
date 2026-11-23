//! Loopback HTTP contract tests for GCP metadata server and service account impersonation.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use open_ci_core::{Clock, ManualClock, SecretString};
use open_ci_provider::ProviderError;
use open_ci_provider_gcp::{
    GcpTokenSource, ImpersonatedTokenSource, ImpersonationConfig, MetadataTokenSource,
    StaticTokenSource,
};
use reqwest::Client;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

async fn spawn_mock_server(app: Router) -> Option<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(addr)
}

#[derive(Clone, Default)]
struct MetadataState {
    calls: Arc<AtomicUsize>,
    reject: bool,
}

fn mock_metadata_router(state: MetadataState) -> Router {
    Router::new()
        .route(
            "/computeMetadata/v1/instance/service-accounts/default/token",
            get(
                |State(st): State<MetadataState>, headers: HeaderMap| async move {
                    let flavor = headers
                        .get("metadata-flavor")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if flavor != "Google" {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": "missing flavor" })),
                        );
                    }
                    if st.reject {
                        return (StatusCode::FORBIDDEN, Json(json!({ "error": "denied" })));
                    }
                    let call = st.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "access_token": format!("ya29.metadata-token-{call}"),
                            "expires_in": 600,
                            "token_type": "Bearer"
                        })),
                    )
                },
            ),
        )
        .with_state(state)
}

#[derive(Clone, Default)]
struct IamState {
    calls: Arc<AtomicUsize>,
    expected_base_bearer: String,
    reject: bool,
}

fn mock_iam_router(state: IamState) -> Router {
    Router::new()
        .route(
            "/v1/projects/-/serviceAccounts/byoc-runner@tenant-proj.iam.gserviceaccount.com:generateAccessToken",
            post(
                |State(st): State<IamState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    assert_eq!(auth, format!("Bearer {}", st.expected_base_bearer));
                    assert_eq!(
                        body["scope"],
                        json!(["https://www.googleapis.com/auth/compute"])
                    );
                    assert_eq!(body["lifetime"], json!("1800s"));

                    if st.reject {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": { "code": 403, "message": "PERMISSION_DENIED" } })),
                        );
                    }

                    let call = st.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let expire_time = if call == 1 {
                        "2023-11-14T22:23:20Z"
                    } else {
                        "2023-11-14T23:23:20Z"
                    };
                    (
                        StatusCode::OK,
                        Json(json!({
                            "accessToken": format!("ya29.impersonated-{call}"),
                            "expireTime": expire_time
                        })),
                    )
                },
            ),
        )
        .with_state(state)
}

#[tokio::test]
async fn metadata_token_source_fetches_caches_and_refreshes_before_expiry() {
    let state = MetadataState::default();
    let Some(addr) = spawn_mock_server(mock_metadata_router(state.clone())).await else {
        assert_eq!("bind", "");
        return;
    };

    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let source = MetadataTokenSource::with_clock(
        Client::new(),
        format!("http://{addr}"),
        "default",
        Arc::clone(&clock) as Arc<dyn Clock>,
    );

    let first = source.bearer_token().await;
    assert!(first.is_ok());
    if let Ok(tok) = first {
        assert_eq!(tok.expose(), "ya29.metadata-token-1");
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    // 530s elapsed (70s remaining > 60s refresh buffer): served from cache.
    clock.advance(530);
    let cached = source.bearer_token().await;
    assert!(cached.is_ok());
    if let Ok(tok) = cached {
        assert_eq!(tok.expose(), "ya29.metadata-token-1");
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    // 545s elapsed (55s remaining < 60s buffer): refreshes from metadata server.
    clock.advance(15);
    let refreshed = source.bearer_token().await;
    assert!(refreshed.is_ok());
    if let Ok(tok) = refreshed {
        assert_eq!(tok.expose(), "ya29.metadata-token-2");
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn impersonated_token_source_exchanges_caches_and_refreshes() {
    let iam_state = IamState {
        calls: Arc::new(AtomicUsize::new(0)),
        expected_base_bearer: "ya29.base-source".to_owned(),
        reject: false,
    };
    let Some(addr) = spawn_mock_server(mock_iam_router(iam_state.clone())).await else {
        assert_eq!("bind", "");
        return;
    };

    // 1_700_000_000 == 2023-11-14T22:13:20Z; first impersonated token expires at +600s.
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let base = Arc::new(StaticTokenSource(SecretString::new("ya29.base-source")));
    let config = ImpersonationConfig::new("byoc-runner@tenant-proj.iam.gserviceaccount.com")
        .with_lifetime_seconds(1800)
        .with_base_url(format!("http://{addr}"));

    let source = ImpersonatedTokenSource::with_clock(
        base,
        Client::new(),
        config,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );

    let first = source.bearer_token().await;
    assert!(first.is_ok());
    if let Ok(tok) = first {
        assert_eq!(tok.expose(), "ya29.impersonated-1");
    }
    assert_eq!(iam_state.calls.load(Ordering::SeqCst), 1);

    // Cached before 60s refresh window.
    clock.advance(500);
    let cached = source.bearer_token().await;
    assert!(cached.is_ok());
    assert_eq!(iam_state.calls.load(Ordering::SeqCst), 1);

    // Refreshes at 60s buffer boundary.
    clock.advance(40);
    let refreshed = source.bearer_token().await;
    assert!(refreshed.is_ok());
    if let Ok(tok) = refreshed {
        assert_eq!(tok.expose(), "ya29.impersonated-2");
    }
    assert_eq!(iam_state.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn metadata_chained_into_impersonation_supports_cross_project_byoc() {
    let meta_state = MetadataState::default();
    let Some(meta_addr) = spawn_mock_server(mock_metadata_router(meta_state.clone())).await else {
        assert_eq!("bind", "");
        return;
    };

    let iam_state = IamState {
        calls: Arc::new(AtomicUsize::new(0)),
        expected_base_bearer: "ya29.metadata-token-1".to_owned(),
        reject: false,
    };
    let Some(iam_addr) = spawn_mock_server(mock_iam_router(iam_state.clone())).await else {
        assert_eq!("bind", "");
        return;
    };

    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000));
    let metadata = Arc::new(MetadataTokenSource::with_clock(
        Client::new(),
        format!("http://{meta_addr}"),
        "default",
        Arc::clone(&clock),
    ));
    let config = ImpersonationConfig::new("byoc-runner@tenant-proj.iam.gserviceaccount.com")
        .with_lifetime_seconds(1800)
        .with_base_url(format!("http://{iam_addr}"));
    let impersonated = ImpersonatedTokenSource::with_clock(metadata, Client::new(), config, clock);

    let tok = impersonated.bearer_token().await;
    assert!(tok.is_ok());
    if let Ok(secret) = tok {
        assert_eq!(secret.expose(), "ya29.impersonated-1");
    }
    assert_eq!(meta_state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(iam_state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn impersonation_maps_forbidden_to_auth_failed() {
    let iam_state = IamState {
        calls: Arc::new(AtomicUsize::new(0)),
        expected_base_bearer: "ya29.base-source".to_owned(),
        reject: true,
    };
    let Some(addr) = spawn_mock_server(mock_iam_router(iam_state)).await else {
        assert_eq!("bind", "");
        return;
    };

    let base = Arc::new(StaticTokenSource(SecretString::new("ya29.base-source")));
    let config = ImpersonationConfig::new("byoc-runner@tenant-proj.iam.gserviceaccount.com")
        .with_lifetime_seconds(1800)
        .with_base_url(format!("http://{addr}"));
    let source = ImpersonatedTokenSource::with_clock(
        base,
        Client::new(),
        config,
        Arc::new(ManualClock::new(1_700_000_000)),
    );

    let res = source.bearer_token().await;
    assert!(matches!(res, Err(ProviderError::AuthFailed)));
}
