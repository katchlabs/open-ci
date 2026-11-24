//! Verifier for the GitHub Actions runtime token (`ACTIONS_RUNTIME_TOKEN`) presented to cache RPCs.
//!
//! The token's `ac` claim is a JSON-encoded list of `{"Scope": "<ref>", "Permission": <bits>}`
//! grants (bit 1 = read, bit 2 = write): a job may write only its own ref and read its own ref
//! plus the refs GitHub lets it fall back to (base and default branch).

use crate::jwks::{JwksError, RemoteJwks};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use open_ci_core::config::RepositoryAllowlist;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

const MAX_TOKEN_LEN: usize = 16_384;
const MAX_SCOPE_LEN: usize = 512;
const MAX_GRANTS: usize = 32;
const READ: u32 = 1;
const WRITE: u32 = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeTokenError {
    #[error("missing or malformed runtime token")]
    Malformed,

    #[error("runtime token signing keys are unavailable")]
    JwksUnavailable,

    #[error("runtime token signature or standard claims are invalid")]
    InvalidSignature,

    #[error("runtime token has no valid cache scopes")]
    InvalidCacheScopes,

    #[error("runtime token repository is not allowed to use this cache")]
    RepositoryNotAllowed,
}

/// Trust settings for runtime tokens.
#[derive(Clone, Debug)]
pub struct RuntimeTokenPolicy {
    pub issuer: String,
    pub jwks_url: Option<String>,
    pub audience: Option<String>,
    pub repositories: RepositoryAllowlist,
}

/// A cache ref scope a token grants, e.g. `refs/heads/main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheScopeGrant {
    scope: String,
    permission: u32,
}

impl CacheScopeGrant {
    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn can_read(&self) -> bool {
        self.permission & READ != 0
    }

    fn can_write(&self) -> bool {
        self.permission & WRITE != 0
    }
}

/// Verified caller identity: the repository and the ref scopes it may read and write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeIdentity {
    repository_id: u64,
    grants: Vec<CacheScopeGrant>,
}

impl RuntimeIdentity {
    pub fn repository_id(&self) -> u64 {
        self.repository_id
    }

    /// The single scope saves go to: the first writable grant.
    pub fn write_scope(&self) -> Option<&str> {
        self.grants
            .iter()
            .find(|grant| grant.can_write())
            .map(CacheScopeGrant::scope)
    }

    /// Readable scopes in restore order: read-write (the job's own ref) before read-only.
    pub fn read_scopes(&self) -> Vec<&str> {
        let mut readable: Vec<&CacheScopeGrant> = self
            .grants
            .iter()
            .filter(|grant| grant.can_read())
            .collect();
        readable.sort_by_key(|grant| std::cmp::Reverse(grant.permission));
        readable.into_iter().map(CacheScopeGrant::scope).collect()
    }
}

#[derive(Deserialize)]
struct RawClaims {
    #[serde(default)]
    ac: Option<String>,
    #[serde(default)]
    repository_id: Option<RepositoryIdClaim>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RepositoryIdClaim {
    Text(String),
    Number(u64),
}

#[derive(Deserialize)]
struct RawGrant {
    #[serde(rename = "Scope")]
    scope: String,
    #[serde(rename = "Permission")]
    permission: u32,
}

/// Verifies runtime tokens against the issuer's JWKS and the configured repository allowlist.
#[derive(Debug)]
pub struct RuntimeTokenVerifier {
    policy: RuntimeTokenPolicy,
    jwks: RemoteJwks,
}

impl RuntimeTokenVerifier {
    pub fn new(policy: RuntimeTokenPolicy, client: Client) -> Self {
        let jwks = match &policy.jwks_url {
            Some(url) => RemoteJwks::new(url.clone(), client),
            None => RemoteJwks::for_issuer(&policy.issuer, client),
        };
        Self { policy, jwks }
    }

    pub async fn verify(&self, token: &str) -> Result<RuntimeIdentity, RuntimeTokenError> {
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(RuntimeTokenError::Malformed);
        }
        let header = decode_header(token).map_err(|_| RuntimeTokenError::Malformed)?;
        if header.alg != Algorithm::RS256 {
            return Err(RuntimeTokenError::InvalidSignature);
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 128 && kid.is_ascii())
            .ok_or(RuntimeTokenError::Malformed)?;
        let jwk = self.jwks.key(&kid).await.map_err(|err| match err {
            JwksError::Unavailable => RuntimeTokenError::JwksUnavailable,
            JwksError::UnknownKey => RuntimeTokenError::InvalidSignature,
        })?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| RuntimeTokenError::InvalidSignature)?;
        let claims = decode::<RawClaims>(token, &key, &self.validation())
            .map_err(|_| RuntimeTokenError::InvalidSignature)?
            .claims;
        self.identity(claims)
    }

    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.policy.issuer.as_str()]);
        validation.validate_nbf = true;
        validation.leeway = 10;
        if let Some(audience) = &self.policy.audience {
            validation.set_audience(&[audience.as_str()]);
            validation.set_required_spec_claims(&["iss", "exp", "aud"]);
        } else {
            validation.validate_aud = false;
            validation.set_required_spec_claims(&["iss", "exp"]);
        }
        validation
    }

    fn identity(&self, claims: RawClaims) -> Result<RuntimeIdentity, RuntimeTokenError> {
        let repository_id = match claims.repository_id {
            Some(RepositoryIdClaim::Number(id)) => Some(id),
            Some(RepositoryIdClaim::Text(raw)) => raw.parse().ok(),
            None => None,
        }
        .ok_or(RuntimeTokenError::RepositoryNotAllowed)?;
        if !self.policy.repositories.allows(repository_id) {
            return Err(RuntimeTokenError::RepositoryNotAllowed);
        }
        let grants = parse_grants(claims.ac.as_deref())?;
        Ok(RuntimeIdentity {
            repository_id,
            grants,
        })
    }
}

fn parse_grants(ac: Option<&str>) -> Result<Vec<CacheScopeGrant>, RuntimeTokenError> {
    let raw: Vec<RawGrant> = ac
        .filter(|ac| !ac.is_empty())
        .and_then(|ac| serde_json::from_str(ac).ok())
        .ok_or(RuntimeTokenError::InvalidCacheScopes)?;
    let valid_scope = |scope: &str| {
        !scope.is_empty() && scope.len() <= MAX_SCOPE_LEN && !scope.contains(char::is_control)
    };
    if raw.is_empty()
        || raw.len() > MAX_GRANTS
        || !raw.iter().all(|grant| valid_scope(&grant.scope))
    {
        return Err(RuntimeTokenError::InvalidCacheScopes);
    }
    Ok(raw
        .into_iter()
        .map(|grant| CacheScopeGrant {
            scope: grant.scope,
            permission: grant.permission,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn identity(ac: &str) -> Result<RuntimeIdentity, RuntimeTokenError> {
        let verifier = RuntimeTokenVerifier::new(
            RuntimeTokenPolicy {
                issuer: "https://issuer.example".to_owned(),
                jwks_url: None,
                audience: None,
                repositories: RepositoryAllowlist::Ids(BTreeSet::from([7])),
            },
            Client::new(),
        );
        verifier.identity(RawClaims {
            ac: Some(ac.to_owned()),
            repository_id: Some(RepositoryIdClaim::Text("7".to_owned())),
        })
    }

    #[test]
    fn pull_request_writes_its_own_ref_and_reads_own_ref_before_base() {
        let id = identity(
            r#"[{"Scope":"refs/heads/main","Permission":1},{"Scope":"refs/pull/9/merge","Permission":3}]"#,
        );
        assert!(matches!(&id, Ok(i) if i.write_scope() == Some("refs/pull/9/merge")));
        assert!(
            matches!(&id, Ok(i) if i.read_scopes() == ["refs/pull/9/merge", "refs/heads/main"])
        );
    }

    #[test]
    fn read_only_token_has_no_write_scope() {
        let id = identity(r#"[{"Scope":"refs/heads/main","Permission":1}]"#);
        assert!(
            matches!(&id, Ok(i) if i.write_scope().is_none() && i.read_scopes() == ["refs/heads/main"])
        );
    }

    #[test]
    fn malformed_or_empty_scopes_are_rejected() {
        for ac in [
            "",
            "[]",
            "not json",
            r#"[{"Scope":"","Permission":3}]"#,
            r#"[{"Scope":"a\u0000b","Permission":3}]"#,
        ] {
            assert_eq!(
                identity(ac),
                Err(RuntimeTokenError::InvalidCacheScopes),
                "{ac}"
            );
        }
    }
}
