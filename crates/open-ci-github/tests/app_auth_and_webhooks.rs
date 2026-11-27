use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use open_ci_core::config::OidcTrustConfig;
use open_ci_core::{GitHubScope, ManualClock, SecretString};
use open_ci_github::jwks::RemoteJwks;
use open_ci_github::test_issuer::TestIssuer;
use open_ci_github::webhook::sign_webhook_body;
use open_ci_github::{
    GitHubAppTokenMinter, GitHubAppWebhookEvent, GitHubError, OidcError, RsaPemJwtSigner,
    WebhookError, validate_oidc_policy, verify_and_parse_app_webhook, verify_oidc_signature,
};
use reqwest::Client;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

const TEST_RSA_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/actions-runtime-token-TEST-ONLY.pem"
));

#[derive(Clone, Default)]
struct MockAppApiState {
    calls: Arc<AtomicUsize>,
}

async fn spawn_mock_github_app_api(state: MockAppApiState) -> Option<String> {
    let app = Router::new()
        .route(
            "/app/installations/{id}/access_tokens",
            post(
                |State(st): State<MockAppApiState>, Path(id): Path<u64>, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if !auth.starts_with("Bearer ") || auth.split('.').count() != 3 {
                        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "bad jwt" })));
                    }
                    if id == 404 {
                        return (StatusCode::NOT_FOUND, Json(json!({ "message": "missing" })));
                    }
                    let seq = st.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "token": format!("ghs_inst_{id}_seq_{seq}"),
                            "expires_at": "2023-11-14T23:13:20Z"
                        })),
                    )
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(format!("http://{addr}"))
}

#[tokio::test]
async fn app_token_minter_caches_per_installation_refreshes_on_expiry_and_invalidates() {
    let state = MockAppApiState::default();
    let base_url = spawn_mock_github_app_api(state.clone()).await;
    assert!(base_url.is_some());
    let Some(base_url) = base_url else { return };

    let signer = RsaPemJwtSigner::from_pem(&SecretString::new(TEST_RSA_PEM));
    assert!(signer.is_ok());
    let Ok(signer) = signer else { return };

    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let minter = GitHubAppTokenMinter::with_clock(
        12_345,
        &base_url,
        Client::new(),
        Arc::new(signer),
        Arc::clone(&clock) as _,
    );
    assert!(minter.is_ok());
    let Ok(minter) = minter else { return };
    let minter = Arc::new(minter);

    let src_a = minter.token_source(101);
    let src_b = minter.token_source(202);

    let a1 = src_a.bearer_token().await;
    let a2 = src_a.bearer_token().await;
    let b1 = src_b.bearer_token().await;
    assert!(matches!(a1, Ok(ref t) if t.expose() == "ghs_inst_101_seq_1"));
    assert!(matches!(a2, Ok(ref t) if t.expose() == "ghs_inst_101_seq_1"));
    assert!(matches!(b1, Ok(ref t) if t.expose() == "ghs_inst_202_seq_2"));
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);

    clock.advance(3_500);
    let a3 = src_a.bearer_token().await;
    assert!(matches!(a3, Ok(ref t) if t.expose() == "ghs_inst_101_seq_3"));
    assert_eq!(state.calls.load(Ordering::SeqCst), 3);

    minter.invalidate(202).await;
    let b2 = src_b.bearer_token().await;
    assert!(matches!(b2, Ok(ref t) if t.expose() == "ghs_inst_202_seq_4"));

    assert_eq!(
        minter.installation_token(404).await.err(),
        Some(GitHubError::NotFound)
    );
}

#[test]
fn app_webhook_parses_workflow_job_installation_and_repository_events() {
    let secret = SecretString::new("0123456789abcdef0123456789abcdef");

    let job_body = br#"{"action":"queued","installation":{"id":9001},"organization":{"id":77},"repository":{"id":555,"full_name":"acme/api"},"workflow_job":{"id":42,"run_id":8,"labels":["open-ci-linux-x64"]}}"#;
    let job_sig = sign_webhook_body(&secret, job_body).unwrap_or_default();
    let parsed_job = verify_and_parse_app_webhook(&secret, &job_sig, "workflow_job", job_body);
    assert!(matches!(
        parsed_job,
        Ok(GitHubAppWebhookEvent::WorkflowJob(ev))
            if ev.installation_id == Some(9001)
                && ev.organization_id == Some(77)
                && ev.repository_id == 555
                && ev.job.repository == "acme/api"
                && ev.job.job_id == 42
    ));

    let inst_body = br#"{"action":"created","installation":{"id":9001,"account":{"id":77,"login":"acme","type":"Organization"},"repository_selection":"selected"},"repositories":[{"id":555,"full_name":"acme/api"}]}"#;
    let inst_sig = sign_webhook_body(&secret, inst_body).unwrap_or_default();
    let parsed_inst = verify_and_parse_app_webhook(&secret, &inst_sig, "installation", inst_body);
    assert!(matches!(
        parsed_inst,
        Ok(GitHubAppWebhookEvent::Installation(ev))
            if ev.action == "created"
                && ev.installation_id == 9001
                && ev.account_login == "acme"
                && ev.repositories.len() == 1
    ));

    let repos_body = br#"{"action":"added","installation":{"id":9001},"repository_selection":"selected","repositories_added":[{"id":556,"full_name":"acme/web"}],"repositories_removed":[]}"#;
    let repos_sig = sign_webhook_body(&secret, repos_body).unwrap_or_default();
    let parsed_repos =
        verify_and_parse_app_webhook(&secret, &repos_sig, "installation_repositories", repos_body);
    assert!(matches!(
        parsed_repos,
        Ok(GitHubAppWebhookEvent::InstallationRepositories(ev))
            if ev.action == "added"
                && ev.installation_id == 9001
                && ev.repositories_added.len() == 1
                && ev.repositories_added[0].full_name == "acme/web"
    ));

    assert_eq!(
        verify_and_parse_app_webhook(&secret, "sha256=00", "workflow_job", job_body),
        Err(WebhookError::InvalidSignature)
    );
    assert_eq!(
        verify_and_parse_app_webhook(&secret, &job_sig, "ping", job_body),
        Err(WebhookError::IgnoredEvent("ping".to_owned()))
    );
}

#[tokio::test]
async fn split_oidc_verification_separates_crypto_from_per_tenant_policy() {
    let issuer = TestIssuer::start().await;
    assert!(issuer.is_ok());
    let Ok(issuer) = issuer else { return };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let token = issuer.sign(&json!({
        "iss": issuer.issuer(),
        "aud": "open-ci:hosted",
        "exp": now + 300,
        "nbf": now.saturating_sub(5),
        "sub": "repo:acme/api:pull_request",
        "repository": "acme/api",
        "repository_id": "555",
        "repository_owner_id": "77",
        "run_id": "99",
        "run_attempt": "1",
        "sha": "a".repeat(40),
        "job_workflow_ref": "acme/api/.github/workflows/ci.yml@refs/heads/main",
        "job_workflow_sha": "a".repeat(40),
        "workflow_ref": "acme/api/.github/workflows/ci.yml@refs/heads/main"
    }));
    assert!(token.is_ok());
    let Ok(token) = token else { return };

    let jwks = RemoteJwks::for_issuer(issuer.issuer(), Client::new());
    let verified = verify_oidc_signature(&jwks, issuer.issuer(), "open-ci:hosted", &token).await;
    assert!(matches!(
        verified,
        Ok(ref v) if v.owner_id == 77 && v.repository_id == 555 && v.repository == "acme/api"
    ));
    let Ok(verified) = verified else { return };

    let tenant_acme_trust = OidcTrustConfig {
        audience: "open-ci:hosted".to_owned(),
        repository_id: None,
        owner_id: 77,
        subjects: BTreeSet::from(["repo:acme/*:pull_request".to_owned()]),
        workflows: BTreeMap::new(),
        allowed_workflow_paths: BTreeSet::from([".github/workflows/ci.yml".to_owned()]),
        caller_workflows: BTreeSet::new(),
    };
    let acme_scope = GitHubScope::Organization {
        org: "acme".to_owned(),
        allowed_repositories: vec!["acme/*".to_owned()],
        runner_group_id: None,
    };
    assert!(validate_oidc_policy(&verified, &tenant_acme_trust, &acme_scope).is_ok());

    let other_tenant_scope = GitHubScope::Organization {
        org: "other".to_owned(),
        allowed_repositories: vec!["other/*".to_owned()],
        runner_group_id: None,
    };
    assert_eq!(
        validate_oidc_policy(&verified, &tenant_acme_trust, &other_tenant_scope),
        Err(OidcError::UntrustedClaim("repository"))
    );
}
