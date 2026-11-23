//! Constant-time GitHub webhook HMAC-SHA256 verification and typed payload parsing.

use hmac::{Hmac, Mac};
use open_ci_core::{GitHubScope, SecretString};
use serde::Deserialize;
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WebhookError {
    #[error("missing or invalid webhook signature")]
    InvalidSignature,

    #[error("event `{0}` ignored")]
    IgnoredEvent(String),

    #[error("malformed webhook payload")]
    MalformedPayload,

    #[error("repository `{0}` is not allowlisted")]
    RepositoryMismatch(String),

    #[error("unsupported workflow_job action `{0}`")]
    UnsupportedAction(String),
}

/// Typed subset of a GitHub `workflow_job` webhook payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowJobEvent {
    pub action: String,
    pub repository: String,
    pub job_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub head_sha: String,
    pub labels: Vec<String>,
}

#[derive(Deserialize)]
struct RawWebhook {
    action: String,
    repository: RawRepo,
    workflow_job: RawWorkflowJob,
}

#[derive(Deserialize)]
struct RawRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct RawWorkflowJob {
    id: u64,
    run_id: u64,
    #[serde(default = "default_run_attempt")]
    run_attempt: u32,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    labels: Vec<String>,
}

const fn default_run_attempt() -> u32 {
    1
}

/// Verifies `X-Hub-Signature-256` using constant-time HMAC comparison and parses
/// a `workflow_job` event whose repository is permitted by `scope`.
pub fn verify_and_parse_webhook(
    secret: &SecretString,
    signature_header: &str,
    event_header: &str,
    body: &[u8],
    scope: &GitHubScope,
) -> Result<WorkflowJobEvent, WebhookError> {
    let hex_digest = signature_header
        .strip_prefix("sha256=")
        .ok_or(WebhookError::InvalidSignature)?;
    let provided = hex::decode(hex_digest).map_err(|_| WebhookError::InvalidSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| WebhookError::InvalidSignature)?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| WebhookError::InvalidSignature)?;

    if event_header != "workflow_job" {
        return Err(WebhookError::IgnoredEvent(event_header.to_owned()));
    }

    let raw: RawWebhook =
        serde_json::from_slice(body).map_err(|_| WebhookError::MalformedPayload)?;
    if !scope.allows_repository(&raw.repository.full_name) {
        return Err(WebhookError::RepositoryMismatch(raw.repository.full_name));
    }
    if !matches!(raw.action.as_str(), "queued" | "in_progress" | "completed") {
        return Err(WebhookError::UnsupportedAction(raw.action));
    }
    if raw.workflow_job.id == 0 || raw.workflow_job.run_id == 0 {
        return Err(WebhookError::MalformedPayload);
    }

    Ok(WorkflowJobEvent {
        action: raw.action,
        repository: raw.repository.full_name,
        job_id: raw.workflow_job.id,
        run_id: raw.workflow_job.run_id,
        run_attempt: raw.workflow_job.run_attempt.max(1),
        head_sha: raw.workflow_job.head_sha,
        labels: raw.workflow_job.labels,
    })
}

/// Computes `sha256=<hex>` for signing test webhook requests.
pub fn sign_webhook_body(secret: &SecretString, body: &[u8]) -> Result<String, WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| WebhookError::InvalidSignature)?;
    mac.update(body);
    Ok(format!(
        "sha256={}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_tampered_signature_and_distinguishes_ignored_events() {
        let secret = SecretString::new("0123456789abcdef0123456789abcdef");
        let scope = GitHubScope::Repository("acme/app".to_owned());
        let payload = br#"{"action":"queued","repository":{"full_name":"acme/app"},"workflow_job":{"id":42,"run_id":7,"labels":["self-hosted","open-ci-api"]}}"#;
        let sig = sign_webhook_body(&secret, payload).unwrap_or_default();

        let bad =
            verify_and_parse_webhook(&secret, "sha256=deadbeef", "workflow_job", payload, &scope);
        assert_eq!(bad, Err(WebhookError::InvalidSignature));

        let ping = verify_and_parse_webhook(&secret, &sig, "ping", payload, &scope);
        assert_eq!(ping, Err(WebhookError::IgnoredEvent("ping".to_owned())));

        let ok = verify_and_parse_webhook(&secret, &sig, "workflow_job", payload, &scope);
        assert!(matches!(
            ok,
            Ok(ev) if ev.job_id == 42
                && ev.run_id == 7
                && ev.run_attempt == 1
                && ev.repository == "acme/app"
        ));
    }

    #[test]
    fn org_scope_allows_matching_repos_and_rejects_foreign_orgs() {
        let secret = SecretString::new("0123456789abcdef0123456789abcdef");
        let scope = GitHubScope::Organization {
            org: "acme".to_owned(),
            allowed_repositories: vec!["acme/svc-*".to_owned(), "acme/core".to_owned()],
            runner_group_id: None,
        };

        let allowed = br#"{"action":"queued","repository":{"full_name":"acme/svc-auth"},"workflow_job":{"id":10,"run_id":99,"run_attempt":2,"head_sha":"deadbeef","labels":["self-hosted","open-ci-api"]}}"#;
        let sig = sign_webhook_body(&secret, allowed).unwrap_or_default();
        let parsed = verify_and_parse_webhook(&secret, &sig, "workflow_job", allowed, &scope);
        assert!(matches!(
            parsed,
            Ok(ev) if ev.repository == "acme/svc-auth"
                && ev.run_attempt == 2
                && ev.head_sha == "deadbeef"
        ));

        let blocked_repo = br#"{"action":"queued","repository":{"full_name":"acme/unlisted"},"workflow_job":{"id":11,"run_id":100,"labels":["open-ci-api"]}}"#;
        let sig2 = sign_webhook_body(&secret, blocked_repo).unwrap_or_default();
        assert_eq!(
            verify_and_parse_webhook(&secret, &sig2, "workflow_job", blocked_repo, &scope),
            Err(WebhookError::RepositoryMismatch("acme/unlisted".to_owned()))
        );

        let foreign = br#"{"action":"queued","repository":{"full_name":"evil/svc-auth"},"workflow_job":{"id":12,"run_id":101,"labels":["open-ci-api"]}}"#;
        let sig3 = sign_webhook_body(&secret, foreign).unwrap_or_default();
        assert_eq!(
            verify_and_parse_webhook(&secret, &sig3, "workflow_job", foreign, &scope),
            Err(WebhookError::RepositoryMismatch("evil/svc-auth".to_owned()))
        );
    }
}
