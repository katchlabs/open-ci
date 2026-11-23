//! Loopback HTTP contract tests for AWS `IMDSv2`, STS `AssumeRole`, and `AwsCredentialChain`.

use async_trait::async_trait;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::{get, post, put},
};
use open_ci_core::{ManualClock, SecretString};
use open_ci_provider::ProviderError;
use open_ci_provider_aws::sigv4::AwsCredentials;
use open_ci_provider_aws::{
    AssumeRoleConfig, AwsCredentialChain, AwsCredentialsSource, GceWebIdentityCredentials,
    ImdsAwsCredentials, StaticAwsCredentials, StsAssumeRoleCredentials, WebIdentityConfig,
};
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

#[derive(Debug)]
struct FailingCredentials;

#[async_trait]
impl AwsCredentialsSource for FailingCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        Err(ProviderError::AuthFailed)
    }
}

async fn spawn_mock_server(app: Router) -> Option<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(addr)
}

#[derive(Clone, Default)]
struct ImdsState {
    token_calls: Arc<AtomicUsize>,
    cred_calls: Arc<AtomicUsize>,
}

fn mock_imds_router(state: ImdsState) -> Router {
    Router::new()
        .route(
            "/latest/api/token",
            put(|State(st): State<ImdsState>, headers: HeaderMap| async move {
                let ttl = headers
                    .get("x-aws-ec2-metadata-token-ttl-seconds")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if ttl.is_empty() {
                    return (StatusCode::BAD_REQUEST, String::new());
                }
                st.token_calls.fetch_add(1, Ordering::SeqCst);
                (StatusCode::OK, "imds-v2-session-token".to_owned())
            }),
        )
        .route(
            "/latest/meta-data/iam/security-credentials/",
            get(|headers: HeaderMap| async move {
                let tok = headers
                    .get("x-aws-ec2-metadata-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if tok != "imds-v2-session-token" {
                    return (StatusCode::UNAUTHORIZED, String::new());
                }
                (StatusCode::OK, "open-ci-controller-role\n".to_owned())
            }),
        )
        .route(
            "/latest/meta-data/iam/security-credentials/open-ci-controller-role",
            get(|State(st): State<ImdsState>, headers: HeaderMap| async move {
                let tok = headers
                    .get("x-aws-ec2-metadata-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if tok != "imds-v2-session-token" {
                    return (StatusCode::UNAUTHORIZED, String::new());
                }
                let call = st.cred_calls.fetch_add(1, Ordering::SeqCst) + 1;
                let (key, exp) = if call == 1 {
                    ("ASIA_IMDS_FIRST", "2023-11-14T22:23:20Z")
                } else {
                    ("ASIA_IMDS_SECOND", "2023-11-14T23:23:20Z")
                };
                let body = format!(
                    r#"{{"Code":"Success","AccessKeyId":"{key}","SecretAccessKey":"IMDS_SECRET_{call}","Token":"IMDS_TOK_{call}","Expiration":"{exp}"}}"#
                );
                (StatusCode::OK, body)
            }),
        )
        .with_state(state)
}

#[derive(Clone, Default)]
struct StsState {
    calls: Arc<AtomicUsize>,
    require_session_token: bool,
    reject_access: bool,
}

fn mock_sts_router(state: StsState) -> Router {
    Router::new()
        .route(
            "/",
            post(
                |State(st): State<StsState>, headers: HeaderMap, body: String| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential="));
                    assert!(auth.contains("/us-east-1/sts/aws4_request"));
                    assert!(auth.contains("x-amz-content-sha256"));
                    assert!(headers.get("x-amz-date").is_some());
                    if st.require_session_token {
                        let sec_tok = headers
                            .get("x-amz-security-token")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(sec_tok, "IMDS_TOK_1");
                        assert!(auth.contains("x-amz-security-token"));
                    }
                    assert!(body.contains("Action=AssumeRole"));
                    assert!(body.contains("Version=2011-06-15"));
                    assert!(body.contains(
                        "RoleArn=arn%3Aaws%3Aiam%3A%3A210987654321%3Arole%2Fopen-ci-byoc"
                    ));
                    assert!(body.contains("RoleSessionName=open-ci-controller"));
                    assert!(body.contains("ExternalId=tenant-ext%2Fid-42"));
                    assert!(body.contains("DurationSeconds=1800"));

                    if st.reject_access {
                        return (
                        StatusCode::FORBIDDEN,
                        "<ErrorResponse><Error><Code>AccessDenied</Code></Error></ErrorResponse>"
                            .to_owned(),
                    );
                    }

                    let call = st.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let (key, exp) = if call == 1 {
                        ("ASIA_STS_FIRST", "2023-11-14T22:23:20Z")
                    } else {
                        ("ASIA_STS_SECOND", "2023-11-14T23:23:20Z")
                    };
                    let xml = format!(
                        "<AssumeRoleResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
                     <AssumeRoleResult><Credentials>\
                     <AccessKeyId>{key}</AccessKeyId>\
                     <SecretAccessKey>STS_SECRET_{call}</SecretAccessKey>\
                     <SessionToken>STS_TOK_{call}</SessionToken>\
                     <Expiration>{exp}</Expiration>\
                     </Credentials></AssumeRoleResult></AssumeRoleResponse>"
                    );
                    (StatusCode::OK, xml)
                },
            ),
        )
        .with_state(state)
}

#[tokio::test]
async fn imdsv2_fetches_caches_and_refreshes_before_expiry() {
    let state = ImdsState::default();
    let Some(addr) = spawn_mock_server(mock_imds_router(state.clone())).await else {
        panic_free_fail("failed to bind imds mock server");
        return;
    };

    // 1_700_000_000 == 2023-11-14T22:13:20Z; first credential expires at +600s.
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let imds = ImdsAwsCredentials::with_clock(
        Client::new(),
        format!("http://{addr}"),
        Arc::clone(&clock) as Arc<dyn open_ci_core::Clock>,
    );

    let first = imds.credentials().await;
    assert!(first.is_ok());
    if let Ok(creds) = first {
        assert_eq!(creds.access_key_id, "ASIA_IMDS_FIRST");
        assert_eq!(creds.secret_access_key.expose(), "IMDS_SECRET_1");
        assert_eq!(
            creds.session_token.as_ref().map(SecretString::expose),
            Some("IMDS_TOK_1")
        );
    }
    assert_eq!(state.cred_calls.load(Ordering::SeqCst), 1);

    // Still 100s before expiry (> 60s buffer): must hit cache.
    clock.advance(500);
    let cached = imds.credentials().await;
    assert!(cached.is_ok());
    if let Ok(creds) = cached {
        assert_eq!(creds.access_key_id, "ASIA_IMDS_FIRST");
    }
    assert_eq!(state.cred_calls.load(Ordering::SeqCst), 1);

    // Within 60s refresh window (540s elapsed -> 60s remaining): must refresh.
    clock.advance(40);
    let refreshed = imds.credentials().await;
    assert!(refreshed.is_ok());
    if let Ok(creds) = refreshed {
        assert_eq!(creds.access_key_id, "ASIA_IMDS_SECOND");
        assert_eq!(
            creds.session_token.as_ref().map(SecretString::expose),
            Some("IMDS_TOK_2")
        );
    }
    assert_eq!(state.cred_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sts_assume_role_signs_request_sends_external_id_and_caches() {
    let state = StsState::default();
    let Some(addr) = spawn_mock_server(mock_sts_router(state.clone())).await else {
        panic_free_fail("failed to bind sts mock server");
        return;
    };

    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let base = Arc::new(StaticAwsCredentials(AwsCredentials {
        access_key_id: "AKIA_BASE".to_owned(),
        secret_access_key: SecretString::new("BASE_SECRET"),
        session_token: None,
    }));
    let config = AssumeRoleConfig::new(
        "arn:aws:iam::210987654321:role/open-ci-byoc",
        "open-ci-controller",
    )
    .with_external_id("tenant-ext/id-42")
    .with_duration_seconds(1800)
    .with_sts_endpoint(format!("http://{addr}/"));

    let sts = StsAssumeRoleCredentials::with_clock(
        base,
        Client::new(),
        config,
        Arc::clone(&clock) as Arc<dyn open_ci_core::Clock>,
    );

    let first = sts.credentials().await;
    assert!(first.is_ok());
    if let Ok(creds) = first {
        assert_eq!(creds.access_key_id, "ASIA_STS_FIRST");
        assert_eq!(creds.secret_access_key.expose(), "STS_SECRET_1");
        assert_eq!(
            creds.session_token.as_ref().map(SecretString::expose),
            Some("STS_TOK_1")
        );
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    // Cached before refresh buffer.
    clock.advance(530);
    let cached = sts.credentials().await;
    assert!(cached.is_ok());
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    // Refreshes once inside 60s buffer.
    clock.advance(15);
    let refreshed = sts.credentials().await;
    assert!(refreshed.is_ok());
    if let Ok(creds) = refreshed {
        assert_eq!(creds.access_key_id, "ASIA_STS_SECOND");
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sts_assume_role_maps_access_denied_to_auth_failed() {
    let state = StsState {
        calls: Arc::new(AtomicUsize::new(0)),
        require_session_token: false,
        reject_access: true,
    };
    let Some(addr) = spawn_mock_server(mock_sts_router(state)).await else {
        panic_free_fail("failed to bind sts mock server");
        return;
    };

    let base = Arc::new(StaticAwsCredentials(AwsCredentials {
        access_key_id: "AKIA_BASE".to_owned(),
        secret_access_key: SecretString::new("BASE_SECRET"),
        session_token: None,
    }));
    let config = AssumeRoleConfig::new(
        "arn:aws:iam::210987654321:role/open-ci-byoc",
        "open-ci-controller",
    )
    .with_external_id("tenant-ext/id-42")
    .with_duration_seconds(1800)
    .with_sts_endpoint(format!("http://{addr}/"));

    let sts = StsAssumeRoleCredentials::with_clock(
        base,
        Client::new(),
        config,
        Arc::new(ManualClock::new(1_700_000_000)),
    );

    let err = sts.credentials().await;
    assert!(matches!(err, Err(ProviderError::AuthFailed)));
}

#[tokio::test]
async fn credential_chain_falls_back_to_imds_and_assumes_role() {
    let imds_state = ImdsState::default();
    let Some(imds_addr) = spawn_mock_server(mock_imds_router(imds_state.clone())).await else {
        panic_free_fail("failed to bind imds mock server");
        return;
    };
    let sts_state = StsState {
        calls: Arc::new(AtomicUsize::new(0)),
        require_session_token: true,
        reject_access: false,
    };
    let Some(sts_addr) = spawn_mock_server(mock_sts_router(sts_state.clone())).await else {
        panic_free_fail("failed to bind sts mock server");
        return;
    };

    let clock: Arc<dyn open_ci_core::Clock> = Arc::new(ManualClock::new(1_700_000_000));
    let imds = Arc::new(ImdsAwsCredentials::with_clock(
        Client::new(),
        format!("http://{imds_addr}"),
        Arc::clone(&clock),
    ));
    let assume_cfg = AssumeRoleConfig::new(
        "arn:aws:iam::210987654321:role/open-ci-byoc",
        "open-ci-controller",
    )
    .with_external_id("tenant-ext/id-42")
    .with_duration_seconds(1800)
    .with_sts_endpoint(format!("http://{sts_addr}/"));

    let chain = AwsCredentialChain::with_sources(
        Arc::new(FailingCredentials),
        imds,
        Client::new(),
        Some(assume_cfg),
        clock,
    );

    let creds = chain.credentials().await;
    assert!(creds.is_ok());
    if let Ok(resolved) = creds {
        assert_eq!(resolved.access_key_id, "ASIA_STS_FIRST");
        assert_eq!(
            resolved.session_token.as_ref().map(SecretString::expose),
            Some("STS_TOK_1")
        );
    }
    assert_eq!(imds_state.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sts_state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn credential_chain_prefers_primary_when_available() {
    let imds_state = ImdsState::default();
    let Some(imds_addr) = spawn_mock_server(mock_imds_router(imds_state.clone())).await else {
        panic_free_fail("failed to bind imds mock server");
        return;
    };

    let clock: Arc<dyn open_ci_core::Clock> = Arc::new(ManualClock::new(1_700_000_000));
    let primary = Arc::new(StaticAwsCredentials(AwsCredentials {
        access_key_id: "AKIA_PRIMARY".to_owned(),
        secret_access_key: SecretString::new("PRIMARY_SECRET"),
        session_token: None,
    }));
    let fallback = Arc::new(ImdsAwsCredentials::with_clock(
        Client::new(),
        format!("http://{imds_addr}"),
        Arc::clone(&clock),
    ));

    let chain = AwsCredentialChain::with_sources(primary, fallback, Client::new(), None, clock);
    let creds = chain.credentials().await;
    assert!(creds.is_ok());
    if let Ok(resolved) = creds {
        assert_eq!(resolved.access_key_id, "AKIA_PRIMARY");
    }
    assert_eq!(imds_state.token_calls.load(Ordering::SeqCst), 0);
}

fn panic_free_fail(msg: &str) {
    assert_eq!(msg, "");
}

const FAKE_GCE_ID_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.eyJhdWQiOiJzdHM6b3Blbi1jaSJ9.c2ln";
const WEB_ROLE_ARN: &str = "arn:aws:iam::123456789012:role/open-ci-gcp-controller";

#[derive(Clone)]
struct WebIdentityState {
    metadata_calls: Arc<AtomicUsize>,
    sts_calls: Arc<AtomicUsize>,
    metadata_status: StatusCode,
    metadata_flavor: Option<&'static str>,
    identity_token: &'static str,
    sts_error: Option<(StatusCode, &'static str)>,
}

impl Default for WebIdentityState {
    fn default() -> Self {
        Self {
            metadata_calls: Arc::default(),
            sts_calls: Arc::default(),
            metadata_status: StatusCode::OK,
            metadata_flavor: Some("Google"),
            identity_token: FAKE_GCE_ID_TOKEN,
            sts_error: None,
        }
    }
}

async fn fake_gce_identity(
    State(st): State<WebIdentityState>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    assert_eq!(
        headers.get("metadata-flavor").and_then(|v| v.to_str().ok()),
        Some("Google")
    );
    let query = query.unwrap_or_default();
    assert!(query.contains("audience=sts%3Aopen-ci"));
    assert!(query.contains("format=full"));
    st.metadata_calls.fetch_add(1, Ordering::SeqCst);
    let mut response = (st.metadata_status, st.identity_token).into_response();
    if let Some(flavor) = st.metadata_flavor {
        response
            .headers_mut()
            .insert("metadata-flavor", HeaderValue::from_static(flavor));
    }
    response
}

async fn fake_sts_web_identity(
    State(st): State<WebIdentityState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let call = st.sts_calls.fetch_add(1, Ordering::SeqCst) + 1;
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("x-amz-security-token").is_none());
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/x-www-form-urlencoded; charset=utf-8")
    );
    for expected in [
        "Action=AssumeRoleWithWebIdentity",
        "Version=2011-06-15",
        "RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fopen-ci-gcp-controller",
        "RoleSessionName=open-ci",
        "DurationSeconds=3600",
    ] {
        assert!(body.contains(expected), "missing {expected}");
    }
    assert!(body.contains(&format!("WebIdentityToken={FAKE_GCE_ID_TOKEN}")));
    assert!(!body.contains("ExternalId"));
    if let Some((status, code)) = st.sts_error {
        return (
            status,
            format!("<ErrorResponse><Error><Code>{code}</Code></Error></ErrorResponse>"),
        );
    }
    let exp = if call == 1 {
        "2023-11-14T22:23:20Z"
    } else {
        "2023-11-14T23:23:20Z"
    };
    (
        StatusCode::OK,
        format!(
            "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult>\
             <SubjectFromWebIdentityToken>1234</SubjectFromWebIdentityToken>\
             <Credentials><AccessKeyId>ASIA_WEB_{call}</AccessKeyId>\
             <SecretAccessKey>WEB_SECRET_{call}</SecretAccessKey>\
             <SessionToken>WEB_TOK_{call}</SessionToken>\
             <Expiration>{exp}</Expiration></Credentials>\
             </AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>"
        ),
    )
}

fn web_identity_config(addr: SocketAddr) -> WebIdentityConfig {
    WebIdentityConfig::new(WEB_ROLE_ARN, "open-ci", "sts:open-ci")
        .with_sts_endpoint(format!("http://{addr}/"))
        .with_metadata_base_url(format!("http://{addr}"))
}

async fn spawn_web_identity(state: &WebIdentityState) -> Option<SocketAddr> {
    let app = Router::new()
        .route(
            "/computeMetadata/v1/instance/service-accounts/default/identity",
            get(fake_gce_identity),
        )
        .route("/", post(fake_sts_web_identity))
        .with_state(state.clone());
    spawn_mock_server(app).await
}

async fn web_identity_result(state: WebIdentityState) -> Option<Result<(), ProviderError>> {
    let addr = spawn_web_identity(&state).await?;
    let source = GceWebIdentityCredentials::with_clock(
        Client::new(),
        web_identity_config(addr),
        Arc::new(ManualClock::new(1_700_000_000)),
    );
    Some(source.credentials().await.map(|_| ()))
}

#[tokio::test]
async fn gce_web_identity_exchanges_unsigned_caches_and_refreshes() {
    let state = WebIdentityState::default();
    let Some(addr) = spawn_web_identity(&state).await else {
        panic_free_fail("failed to bind web identity mock server");
        return;
    };
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let source = GceWebIdentityCredentials::with_clock(
        Client::new(),
        web_identity_config(addr),
        Arc::clone(&clock) as Arc<dyn open_ci_core::Clock>,
    );

    let first = source.credentials().await;
    assert!(matches!(&first, Ok(c) if c.access_key_id == "ASIA_WEB_1"
        && c.secret_access_key.expose() == "WEB_SECRET_1"
        && c.session_token.as_ref().map(SecretString::expose) == Some("WEB_TOK_1")));

    clock.advance(500);
    assert!(source.credentials().await.is_ok());
    assert_eq!(state.sts_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.metadata_calls.load(Ordering::SeqCst), 1);

    clock.advance(40);
    let refreshed = source.credentials().await;
    assert!(matches!(&refreshed, Ok(c) if c.access_key_id == "ASIA_WEB_2"));
    assert_eq!(state.sts_calls.load(Ordering::SeqCst), 2);
    assert_eq!(state.metadata_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn gce_web_identity_maps_sts_error_codes() {
    for (status, code, expected) in [
        (
            StatusCode::FORBIDDEN,
            "AccessDenied",
            ProviderError::AuthFailed,
        ),
        (
            StatusCode::BAD_REQUEST,
            "InvalidIdentityToken",
            ProviderError::AuthFailed,
        ),
        (
            StatusCode::BAD_REQUEST,
            "IDPRejectedClaim",
            ProviderError::AuthFailed,
        ),
        (
            StatusCode::BAD_REQUEST,
            "ExpiredTokenException",
            ProviderError::AuthFailed,
        ),
        (
            StatusCode::BAD_REQUEST,
            "Throttling",
            ProviderError::RateLimited {
                retry_after_seconds: 30,
            },
        ),
    ] {
        let state = WebIdentityState {
            sts_error: Some((status, code)),
            ..WebIdentityState::default()
        };
        assert_eq!(
            web_identity_result(state).await,
            Some(Err(expected)),
            "{code}"
        );
    }
}

/// Runs one exchange and reports its error message plus metadata/STS call counts.
async fn rejected_exchange(state: WebIdentityState) -> Option<(String, usize, usize)> {
    let (metadata_calls, sts_calls) = (
        Arc::clone(&state.metadata_calls),
        Arc::clone(&state.sts_calls),
    );
    let message = match web_identity_result(state).await? {
        Err(ProviderError::Transient(message)) => message,
        other => format!("unexpected outcome: {other:?}"),
    };
    Some((
        message,
        metadata_calls.load(Ordering::SeqCst),
        sts_calls.load(Ordering::SeqCst),
    ))
}

#[tokio::test]
async fn gce_web_identity_maps_missing_service_account_to_auth_failed() {
    let state = WebIdentityState {
        metadata_status: StatusCode::NOT_FOUND,
        ..WebIdentityState::default()
    };
    assert_eq!(
        web_identity_result(state).await,
        Some(Err(ProviderError::AuthFailed))
    );
}

#[tokio::test]
async fn gce_web_identity_rejects_responses_not_from_gce_metadata() {
    for flavor in [None, Some("nginx")] {
        let state = WebIdentityState {
            metadata_flavor: flavor,
            ..WebIdentityState::default()
        };
        let outcome = rejected_exchange(state).await;
        assert!(
            matches!(&outcome, Some((msg, 1, 0)) if msg.contains("not from the GCE metadata server")),
            "{flavor:?}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn gce_web_identity_rejects_malformed_identity_tokens_before_sts() {
    for malformed in ["..", "a..b", "a.b", "a.b.c.d", "a.b.c d"] {
        let state = WebIdentityState {
            identity_token: malformed,
            ..WebIdentityState::default()
        };
        let outcome = rejected_exchange(state).await;
        assert!(
            matches!(&outcome, Some((msg, 1, 0)) if msg == "malformed gce identity token"),
            "{malformed}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn gce_web_identity_rejects_invalid_config_before_any_request() {
    let state = WebIdentityState::default();
    let Some(addr) = spawn_web_identity(&state).await else {
        panic_free_fail("failed to bind web identity mock server");
        return;
    };
    let base = web_identity_config(addr);
    for config in [
        WebIdentityConfig {
            role_arn: String::new(),
            ..base.clone()
        },
        WebIdentityConfig {
            role_session_name: " ".to_owned(),
            ..base.clone()
        },
        WebIdentityConfig {
            audience: String::new(),
            ..base.clone()
        },
        base.clone().with_duration_seconds(0),
        base.clone().with_sts_endpoint("not a url"),
        base.clone().with_sts_endpoint("mailto:a@b"),
        base.clone().with_metadata_base_url("file:///etc"),
    ] {
        let source = GceWebIdentityCredentials::new(Client::new(), config);
        assert!(matches!(
            source.credentials().await,
            Err(ProviderError::InvalidConfig(_))
        ));
    }
    assert_eq!(state.metadata_calls.load(Ordering::SeqCst), 0);
}
