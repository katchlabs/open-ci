//! `RuntimeTokenVerifier` against a loopback JWKS issuer with real RS256 signatures.

use open_ci_core::config::{AnyRepository, RepositoryAllowlist};
use open_ci_github::test_issuer::TestIssuer;
use open_ci_github::{RuntimeTokenError, RuntimeTokenPolicy, RuntimeTokenVerifier};
use reqwest::Client;
use serde_json::json;
use std::collections::BTreeSet;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const REPO: u64 = 4242;
const MAIN_READ_PR_WRITE: &[(&str, u32)] = &[("refs/heads/main", 1), ("refs/pull/7/merge", 3)];

fn verifier(issuer: &TestIssuer, repositories: RepositoryAllowlist) -> RuntimeTokenVerifier {
    RuntimeTokenVerifier::new(
        RuntimeTokenPolicy {
            issuer: issuer.issuer().to_owned(),
            jwks_url: None,
            audience: None,
            repositories,
        },
        Client::new(),
    )
}

fn only(repo: u64) -> RepositoryAllowlist {
    RepositoryAllowlist::Ids(BTreeSet::from([repo]))
}

#[tokio::test]
async fn valid_runtime_token_yields_repository_and_ref_scopes() -> TestResult {
    let issuer = TestIssuer::start().await?;
    let identity = verifier(&issuer, only(REPO))
        .verify(&issuer.runtime_token(REPO, MAIN_READ_PR_WRITE)?)
        .await?;
    assert_eq!(identity.repository_id(), REPO);
    assert_eq!(identity.write_scope(), Some("refs/pull/7/merge"));
    assert_eq!(
        identity.read_scopes(),
        ["refs/pull/7/merge", "refs/heads/main"]
    );
    Ok(())
}

#[tokio::test]
async fn repository_outside_allowlist_is_rejected_unless_any_is_configured() -> TestResult {
    let issuer = TestIssuer::start().await?;
    let token = issuer.runtime_token(9999, MAIN_READ_PR_WRITE)?;
    assert_eq!(
        verifier(&issuer, only(REPO)).verify(&token).await,
        Err(RuntimeTokenError::RepositoryNotAllowed)
    );
    let any = RepositoryAllowlist::Any(AnyRepository::Any);
    assert!(verifier(&issuer, any).verify(&token).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn tampered_expired_and_foreign_issuer_tokens_are_rejected() -> TestResult {
    let issuer = TestIssuer::start().await?;
    let verifier = verifier(&issuer, only(REPO));
    let good = issuer.runtime_token(REPO, MAIN_READ_PR_WRITE)?;

    let escalated = issuer.runtime_token(REPO, &[("refs/heads/main", 3)])?;
    let (good_head, good_sig) = (good.split('.').next(), good.rsplit('.').next());
    let escalated_payload = escalated.split('.').nth(1);
    let spliced = format!(
        "{}.{}.{}",
        good_head.ok_or("header")?,
        escalated_payload.ok_or("payload")?,
        good_sig.ok_or("signature")?
    );
    assert_eq!(
        verifier.verify(&spliced).await,
        Err(RuntimeTokenError::InvalidSignature),
        "write-main payload spliced under a read-only token's signature"
    );

    let expired = issuer.sign(&json!({"iss": issuer.issuer(), "exp": 1_000_u64,
        "ac": r#"[{"Scope":"refs/heads/main","Permission":3}]"#, "repository_id": REPO.to_string()}))?;
    assert_eq!(
        verifier.verify(&expired).await,
        Err(RuntimeTokenError::InvalidSignature)
    );

    let foreign = issuer.sign(&json!({"iss": "https://evil.example", "exp": 4_102_444_800_u64,
        "ac": r#"[{"Scope":"refs/heads/main","Permission":3}]"#, "repository_id": REPO.to_string()}))?;
    assert_eq!(
        verifier.verify(&foreign).await,
        Err(RuntimeTokenError::InvalidSignature)
    );

    assert_eq!(
        verifier.verify("not-a-jwt").await,
        Err(RuntimeTokenError::Malformed)
    );
    Ok(())
}

#[tokio::test]
async fn configured_audience_is_required() -> TestResult {
    let issuer = TestIssuer::start().await?;
    let verifier = RuntimeTokenVerifier::new(
        RuntimeTokenPolicy {
            issuer: issuer.issuer().to_owned(),
            jwks_url: None,
            audience: Some("open-ci-cache".to_owned()),
            repositories: only(REPO),
        },
        Client::new(),
    );
    let without_aud = issuer.runtime_token(REPO, MAIN_READ_PR_WRITE)?;
    assert_eq!(
        verifier.verify(&without_aud).await,
        Err(RuntimeTokenError::InvalidSignature)
    );

    let with_aud = issuer.sign(&json!({"iss": issuer.issuer(), "exp": 4_102_444_800_u64, "aud": "open-ci-cache",
        "ac": r#"[{"Scope":"refs/heads/main","Permission":1}]"#, "repository_id": REPO.to_string()}))?;
    assert!(verifier.verify(&with_aud).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn unreachable_jwks_fails_closed() -> TestResult {
    let issuer = TestIssuer::start().await?;
    let token = issuer.runtime_token(REPO, MAIN_READ_PR_WRITE)?;
    let verifier = RuntimeTokenVerifier::new(
        RuntimeTokenPolicy {
            issuer: issuer.issuer().to_owned(),
            jwks_url: Some("http://127.0.0.1:9/.well-known/jwks".to_owned()),
            audience: None,
            repositories: only(REPO),
        },
        Client::new(),
    );
    assert_eq!(
        verifier.verify(&token).await,
        Err(RuntimeTokenError::JwksUnavailable)
    );
    Ok(())
}
