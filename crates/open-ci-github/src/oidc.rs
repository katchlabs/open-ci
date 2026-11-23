//! GitHub Actions OIDC RS256 JWT verifier with lock-free JWKS network refresh.

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use open_ci_core::GitHubScope;
use open_ci_core::config::OidcTrustConfig;
use open_ci_core::models::repo_matches_pattern;
use reqwest::Client;
use serde::Deserialize;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

const MAX_JWKS_BYTES: usize = 64 * 1024;
const FORCED_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

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

#[derive(Debug)]
pub struct OidcVerifier {
    issuer: String,
    trust: OidcTrustConfig,
    scope: GitHubScope,
    client: Client,
    jwks_cache: RwLock<Option<(JwkSet, Instant)>>,
    refresh_lock: Mutex<()>,
    cache_ttl: Duration,
}

impl OidcVerifier {
    pub fn new(issuer: String, trust: OidcTrustConfig, scope: GitHubScope, client: Client) -> Self {
        Self {
            issuer,
            trust,
            scope,
            client,
            jwks_cache: RwLock::new(None),
            refresh_lock: Mutex::new(()),
            cache_ttl: Duration::from_secs(300),
        }
    }

    async fn jwks(&self, force_refresh: bool) -> Result<JwkSet, OidcError> {
        {
            let guard = self.jwks_cache.read().await;
            if let Some((set, fetched_at)) = guard.as_ref() {
                let elapsed = fetched_at.elapsed();
                if (!force_refresh && elapsed < self.cache_ttl)
                    || (force_refresh && elapsed < FORCED_REFRESH_COOLDOWN)
                {
                    return Ok(set.clone());
                }
            }
        }

        // Singleflight mutex coalesces concurrent refreshes without blocking readers.
        let _refresh_guard = self.refresh_lock.lock().await;
        {
            let guard = self.jwks_cache.read().await;
            if let Some((set, fetched_at)) = guard.as_ref() {
                let elapsed = fetched_at.elapsed();
                if (!force_refresh && elapsed < self.cache_ttl)
                    || (force_refresh && elapsed < FORCED_REFRESH_COOLDOWN)
                {
                    return Ok(set.clone());
                }
            }
        }

        let url = format!("{}/.well-known/jwks", self.issuer);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| OidcError::JwksUnavailable)?;
        if !resp.status().is_success() {
            return Err(OidcError::JwksUnavailable);
        }
        let body = resp.bytes().await.map_err(|_| OidcError::JwksUnavailable)?;
        if body.len() > MAX_JWKS_BYTES {
            return Err(OidcError::JwksUnavailable);
        }
        let jwks: JwkSet = serde_json::from_slice(&body).map_err(|_| OidcError::JwksUnavailable)?;
        *self.jwks_cache.write().await = Some((jwks.clone(), Instant::now()));
        Ok(jwks)
    }

    pub async fn verify(&self, token: &str) -> Result<OidcClaims, OidcError> {
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

        let mut jwks = self.jwks(false).await?;
        let mut jwk = jwks.find(&kid).cloned();
        if jwk.is_none() {
            jwks = self.jwks(true).await?;
            jwk = jwks.find(&kid).cloned();
        }
        let jwk = jwk.ok_or(OidcError::InvalidSignature)?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| OidcError::InvalidSignature)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.trust.audience.as_str()]);
        validation.validate_nbf = true;
        validation.leeway = 10;

        let data = decode::<RawOidcClaims>(token, &key, &validation)
            .map_err(|_| OidcError::InvalidSignature)?;
        self.validate_claims(data.claims)
    }

    fn validate_claims(&self, c: RawOidcClaims) -> Result<OidcClaims, OidcError> {
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

        if !self.scope.allows_repository(&c.repository) {
            return Err(OidcError::UntrustedClaim("repository"));
        }
        if owner_id != self.trust.owner_id {
            return Err(OidcError::UntrustedClaim("repository_or_owner"));
        }
        if let Some(expected_repo_id) = self.trust.repository_id
            && repository_id != expected_repo_id
        {
            return Err(OidcError::UntrustedClaim("repository_or_owner"));
        }
        if !self
            .trust
            .subjects
            .iter()
            .any(|pattern| subject_matches(pattern, &c.sub, &c.repository))
        {
            return Err(OidcError::UntrustedClaim("subject"));
        }

        let job_wf_ref = c
            .job_workflow_ref
            .as_deref()
            .ok_or(OidcError::UntrustedClaim("job_workflow_ref"))?;

        let matched_pinned = if let Some(allowed_shas) = self.trust.workflows.get(job_wf_ref) {
            let job_wf_sha = c
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
            && !self.trust.allowed_workflow_paths.is_empty()
            && extract_repo_workflow_path(job_wf_ref, &c.repository)
                .is_some_and(|path| self.trust.allowed_workflow_paths.contains(path));

        if !matched_pinned && !matched_repo_path {
            return Err(OidcError::UntrustedClaim("workflow_ref"));
        }

        if !self.trust.caller_workflows.is_empty() {
            let caller = c
                .workflow_ref
                .as_deref()
                .and_then(|wf| extract_repo_workflow_path(wf, &c.repository))
                .ok_or(OidcError::UntrustedClaim("caller_workflow"))?;
            if !self.trust.caller_workflows.contains(caller) {
                return Err(OidcError::UntrustedClaim("caller_workflow"));
            }
        }

        Ok(OidcClaims {
            repository: c.repository,
            repository_id,
            owner_id,
            run_id,
            run_attempt,
            sha: c.sha,
            subject: c.sub,
            workflow_ref: job_wf_ref.to_owned(),
        })
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
