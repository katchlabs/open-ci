//! GitHub Actions OIDC RS256 JWT verifier with lock-free JWKS network refresh.

use crate::jwks::{JwksError, RemoteJwks};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use open_ci_core::GitHubScope;
use open_ci_core::config::OidcTrustConfig;
use open_ci_core::models::repo_matches_pattern;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OidcError {
    #[error("missing or malformed jwt")]
    MalformedToken,

    #[error("failed fetching or parsing oidc jwks")]
    JwksUnavailable,

    #[error("jwt signature or standard claim verification failed")]
    InvalidSignature,

    #[error("oidc claim rejected: {0}")]
    UntrustedClaim(&'static str),
}

/// Verified GitHub Actions OIDC claims required for run admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcClaims {
    pub repository: String,
    pub repository_id: u64,
    pub owner_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub sha: String,
    pub subject: String,
    pub workflow_ref: String,
}

/// Cryptographically verified GitHub Actions OIDC token prior to tenant policy checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedOidcToken {
    pub repository: String,
    pub repository_id: u64,
    pub owner_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub sha: String,
    pub subject: String,
    pub job_workflow_ref: Option<String>,
    pub job_workflow_sha: Option<String>,
    pub workflow_ref: Option<String>,
}

#[derive(Deserialize)]
struct RawOidcClaims {
    sub: String,
    repository: String,
    repository_id: String,
    repository_owner_id: String,
    run_id: String,
    run_attempt: String,
    sha: String,
    #[serde(default)]
    job_workflow_ref: Option<String>,
    #[serde(default)]
    job_workflow_sha: Option<String>,
    #[serde(default)]
    workflow_ref: Option<String>,
}

impl TryFrom<RawOidcClaims> for VerifiedOidcToken {
    type Error = OidcError;

    fn try_from(c: RawOidcClaims) -> Result<Self, Self::Error> {
        let repository_id: u64 = c
            .repository_id
            .parse()
            .map_err(|_| OidcError::UntrustedClaim("repository_id"))?;
        let owner_id: u64 = c
            .repository_owner_id
            .parse()
            .map_err(|_| OidcError::UntrustedClaim("owner_id"))?;
        let run_id: u64 = c
            .run_id
            .parse()
            .map_err(|_| OidcError::UntrustedClaim("run_id"))?;
        let run_attempt: u32 = c
            .run_attempt
            .parse()
            .map_err(|_| OidcError::UntrustedClaim("run_attempt"))?;

        Ok(Self {
            repository: c.repository,
            repository_id,
            owner_id,
            run_id,
            run_attempt,
            sha: c.sha,
            subject: c.sub,
            job_workflow_ref: c.job_workflow_ref,
            job_workflow_sha: c.job_workflow_sha,
            workflow_ref: c.workflow_ref,
        })
    }
}

/// Verifies RS256 signature, issuer, audience, expiry, and parses numeric GitHub IDs
/// before tenant-specific trust policy evaluation.
pub async fn verify_oidc_signature(
    jwks: &RemoteJwks,
    issuer: &str,
    audience: &str,
    token: &str,
) -> Result<VerifiedOidcToken, OidcError> {
    if token.is_empty() || token.len() > 16_384 {
        return Err(OidcError::MalformedToken);
    }
    let header = decode_header(token).map_err(|_| OidcError::MalformedToken)?;
    if header.alg != Algorithm::RS256 {
        return Err(OidcError::InvalidSignature);
    }
    let kid = header.kid.ok_or(OidcError::MalformedToken)?;
    if kid.is_empty() || kid.len() > 128 || !kid.is_ascii() {
        return Err(OidcError::MalformedToken);
    }

    let jwk = jwks.key(&kid).await.map_err(|err| match err {
        JwksError::Unavailable => OidcError::JwksUnavailable,
        JwksError::UnknownKey => OidcError::InvalidSignature,
    })?;
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| OidcError::InvalidSignature)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.validate_nbf = true;
    validation.leeway = 10;

    let data = decode::<RawOidcClaims>(token, &key, &validation)
        .map_err(|_| OidcError::InvalidSignature)?;
    VerifiedOidcToken::try_from(data.claims)
}

/// Enforces tenant-scoped repository, owner, subject, and workflow trust rules against
/// a cryptographically verified [`VerifiedOidcToken`].
pub fn validate_oidc_policy(
    token: &VerifiedOidcToken,
    trust: &OidcTrustConfig,
    scope: &GitHubScope,
) -> Result<OidcClaims, OidcError> {
    if !scope.allows_repository(&token.repository) {
        return Err(OidcError::UntrustedClaim("repository"));
    }
    if token.owner_id != trust.owner_id {
        return Err(OidcError::UntrustedClaim("repository_or_owner"));
    }
    if let Some(expected_repo_id) = trust.repository_id
        && token.repository_id != expected_repo_id
    {
        return Err(OidcError::UntrustedClaim("repository_or_owner"));
    }
    if !trust
        .subjects
        .iter()
        .any(|pattern| subject_matches(pattern, &token.subject, &token.repository))
    {
        return Err(OidcError::UntrustedClaim("subject"));
    }

    let job_wf_ref = token
        .job_workflow_ref
        .as_deref()
        .ok_or(OidcError::UntrustedClaim("job_workflow_ref"))?;

    let matched_pinned = if let Some(allowed_shas) = trust.workflows.get(job_wf_ref) {
        let job_wf_sha = token
            .job_workflow_sha
            .as_deref()
            .ok_or(OidcError::UntrustedClaim("job_workflow_sha"))?;
        if !allowed_shas.contains(job_wf_sha) {
            return Err(OidcError::UntrustedClaim("workflow_sha"));
        }
        true
    } else {
        false
    };

    let matched_repo_path = !matched_pinned
        && !trust.allowed_workflow_paths.is_empty()
        && extract_repo_workflow_path(job_wf_ref, &token.repository)
            .is_some_and(|path| trust.allowed_workflow_paths.contains(path));

    if !matched_pinned && !matched_repo_path {
        return Err(OidcError::UntrustedClaim("workflow_ref"));
    }

    if !trust.caller_workflows.is_empty() {
        let caller = token
            .workflow_ref
            .as_deref()
            .and_then(|wf| extract_repo_workflow_path(wf, &token.repository))
            .ok_or(OidcError::UntrustedClaim("caller_workflow"))?;
        if !trust.caller_workflows.contains(caller) {
            return Err(OidcError::UntrustedClaim("caller_workflow"));
        }
    }

    Ok(OidcClaims {
        repository: token.repository.clone(),
        repository_id: token.repository_id,
        owner_id: token.owner_id,
        run_id: token.run_id,
        run_attempt: token.run_attempt,
        sha: token.sha.clone(),
        subject: token.subject.clone(),
        workflow_ref: job_wf_ref.to_owned(),
    })
}

#[derive(Debug)]
pub struct OidcVerifier {
    issuer: String,
    trust: OidcTrustConfig,
    scope: GitHubScope,
    jwks: RemoteJwks,
}

impl OidcVerifier {
    pub fn new(issuer: String, trust: OidcTrustConfig, scope: GitHubScope, client: Client) -> Self {
        let jwks = RemoteJwks::for_issuer(&issuer, client);
        Self {
            issuer,
            trust,
            scope,
            jwks,
        }
    }

    pub async fn verify(&self, token: &str) -> Result<OidcClaims, OidcError> {
        let verified =
            verify_oidc_signature(&self.jwks, &self.issuer, &self.trust.audience, token).await?;
        validate_oidc_policy(&verified, &self.trust, &self.scope)
    }

    #[cfg(test)]
    fn validate_claims(&self, c: RawOidcClaims) -> Result<OidcClaims, OidcError> {
        let verified = VerifiedOidcToken::try_from(c)?;
        validate_oidc_policy(&verified, &self.trust, &self.scope)
    }
}

fn subject_matches(pattern: &str, subject: &str, repository: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == subject;
    }
    let Some(pat_rest) = pattern.strip_prefix("repo:") else {
        return false;
    };
    let Some(sub_rest) = subject.strip_prefix("repo:") else {
        return false;
    };
    let Some((pat_repo, pat_ctx)) = pat_rest.split_once(':') else {
        return false;
    };
    let Some((sub_repo, sub_ctx)) = sub_rest.split_once(':') else {
        return false;
    };
    !pat_ctx.is_empty()
        && !pat_ctx.contains('*')
        && sub_repo == repository
        && repo_matches_pattern(sub_repo, pat_repo)
        && sub_ctx == pat_ctx
}

fn extract_repo_workflow_path<'a>(workflow_ref: &'a str, repository: &str) -> Option<&'a str> {
    workflow_ref
        .strip_prefix(repository)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.split('@').next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn sample_raw_claims(repo: &str, repo_id: &str, owner_id: &str, sub: &str) -> RawOidcClaims {
        RawOidcClaims {
            sub: sub.to_owned(),
            repository: repo.to_owned(),
            repository_id: repo_id.to_owned(),
            repository_owner_id: owner_id.to_owned(),
            run_id: "42".to_owned(),
            run_attempt: "1".to_owned(),
            sha: "a".repeat(40),
            job_workflow_ref: Some(format!("{repo}/.github/workflows/ci.yml@refs/heads/main")),
            job_workflow_sha: Some("a".repeat(40)),
            workflow_ref: Some(format!("{repo}/.github/workflows/ci.yml@refs/heads/main")),
        }
    }

    #[test]
    fn org_oidc_accepts_owner_id_and_repo_patterns_without_pinned_repo_id() {
        let trust = OidcTrustConfig {
            audience: "open-ci:acme".to_owned(),
            repository_id: None,
            owner_id: 987_654,
            subjects: BTreeSet::from(["repo:acme/*:pull_request".to_owned()]),
            workflows: BTreeMap::new(),
            allowed_workflow_paths: BTreeSet::from([".github/workflows/ci.yml".to_owned()]),
            caller_workflows: BTreeSet::new(),
        };
        let scope = GitHubScope::Organization {
            org: "acme".to_owned(),
            allowed_repositories: vec!["acme/svc-*".to_owned()],
            runner_group_id: None,
        };
        let verifier = OidcVerifier::new(
            "https://token.actions.githubusercontent.com".to_owned(),
            trust,
            scope,
            Client::new(),
        );

        let ok = verifier.validate_claims(sample_raw_claims(
            "acme/svc-payments",
            "111",
            "987654",
            "repo:acme/svc-payments:pull_request",
        ));
        assert!(
            matches!(ok, Ok(c) if c.repository == "acme/svc-payments" && c.repository_id == 111)
        );

        // Different repo in the same org with different repository_id also succeeds.
        let ok2 = verifier.validate_claims(sample_raw_claims(
            "acme/svc-billing",
            "222",
            "987654",
            "repo:acme/svc-billing:pull_request",
        ));
        assert!(ok2.is_ok());

        // Repo outside allowlist is rejected.
        let not_allowed = verifier.validate_claims(sample_raw_claims(
            "acme/other",
            "333",
            "987654",
            "repo:acme/other:pull_request",
        ));
        assert_eq!(not_allowed, Err(OidcError::UntrustedClaim("repository")));

        // Wrong owner_id is rejected even if repository name matches.
        let wrong_owner = verifier.validate_claims(sample_raw_claims(
            "acme/svc-payments",
            "111",
            "999999",
            "repo:acme/svc-payments:pull_request",
        ));
        assert_eq!(
            wrong_owner,
            Err(OidcError::UntrustedClaim("repository_or_owner"))
        );

        // Wrong subject context is rejected.
        let wrong_sub = verifier.validate_claims(sample_raw_claims(
            "acme/svc-payments",
            "111",
            "987654",
            "repo:acme/svc-payments:ref:refs/heads/main",
        ));
        assert_eq!(wrong_sub, Err(OidcError::UntrustedClaim("subject")));
    }

    #[test]
    fn single_repo_oidc_enforces_pinned_repository_id() {
        let trust = OidcTrustConfig {
            audience: "open-ci:acme/app".to_owned(),
            repository_id: Some(777),
            owner_id: 100,
            subjects: BTreeSet::from(["repo:acme/app:pull_request".to_owned()]),
            workflows: BTreeMap::new(),
            allowed_workflow_paths: BTreeSet::from([".github/workflows/ci.yml".to_owned()]),
            caller_workflows: BTreeSet::new(),
        };
        let verifier = OidcVerifier::new(
            "https://token.actions.githubusercontent.com".to_owned(),
            trust,
            GitHubScope::Repository("acme/app".to_owned()),
            Client::new(),
        );

        assert!(
            verifier
                .validate_claims(sample_raw_claims(
                    "acme/app",
                    "777",
                    "100",
                    "repo:acme/app:pull_request",
                ))
                .is_ok()
        );
        assert_eq!(
            verifier.validate_claims(sample_raw_claims(
                "acme/app",
                "888",
                "100",
                "repo:acme/app:pull_request",
            )),
            Err(OidcError::UntrustedClaim("repository_or_owner"))
        );
    }
}
