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
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationRepo {
    pub id: u64,
    pub full_name: String,
}

/// Multi-tenant `workflow_job` event carrying installation and repository IDs for routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppWorkflowJobEvent {
    pub installation_id: Option<u64>,
    pub repository_id: u64,
    pub organization_id: Option<u64>,
    pub job: WorkflowJobEvent,
}

/// Typed subset of a GitHub App `installation` lifecycle webhook payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationEvent {
    pub action: String,
    pub installation_id: u64,
    pub account_login: String,
    pub account_id: u64,
    pub account_type: String,
    pub repository_selection: String,
    pub repositories: Vec<InstallationRepo>,
}

/// Typed subset of a GitHub App `installation_repositories` webhook payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationRepositoriesEvent {
    pub action: String,
    pub installation_id: u64,
    pub repository_selection: String,
    pub repositories_added: Vec<InstallationRepo>,
    pub repositories_removed: Vec<InstallationRepo>,
}

/// Supported multi-tenant GitHub App webhook events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitHubAppWebhookEvent {
    WorkflowJob(AppWorkflowJobEvent),
    Installation(InstallationEvent),
    InstallationRepositories(InstallationRepositoriesEvent),
}

#[derive(Deserialize)]
struct RawWebhook {
    action: String,
    repository: RawRepo,
    workflow_job: RawWorkflowJob,
    #[serde(default)]
    installation: Option<RawInstallationRef>,
    #[serde(default)]
    organization: Option<RawOrgRef>,
}

#[derive(Deserialize)]
struct RawInstallationRef {
    id: u64,
}

#[derive(Deserialize)]
struct RawOrgRef {
    id: u64,
}

#[derive(Deserialize)]
struct RawRepo {
    #[serde(default)]
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct RawInstallationAccount {
    id: u64,
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Deserialize)]
struct RawInstallationBody {
    id: u64,
    account: RawInstallationAccount,
    #[serde(default)]
    repository_selection: String,
}

#[derive(Deserialize)]
struct RawInstallationWebhook {
    action: String,
    installation: RawInstallationBody,
    #[serde(default)]
    repositories: Vec<RawRepo>,
}

#[derive(Deserialize)]
struct RawInstallationReposWebhook {
    action: String,
    installation: RawInstallationRef,
    #[serde(default)]
    repository_selection: String,
    #[serde(default)]
    repositories_added: Vec<RawRepo>,
    #[serde(default)]
    repositories_removed: Vec<RawRepo>,
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
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    runner_name: Option<String>,
    #[serde(default)]
    runner_id: Option<u64>,
}

const fn default_run_attempt() -> u32 {
    1
}

/// Verifies `X-Hub-Signature-256` using constant-time HMAC-SHA256 comparison.
pub fn verify_webhook_signature(
    secret: &SecretString,
    signature_header: &str,
    body: &[u8],
) -> Result<(), WebhookError> {
    let hex_digest = signature_header
        .strip_prefix("sha256=")
        .ok_or(WebhookError::InvalidSignature)?;
    let provided = hex::decode(hex_digest).map_err(|_| WebhookError::InvalidSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| WebhookError::InvalidSignature)?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| WebhookError::InvalidSignature)
}

fn parse_workflow_job_raw(raw: RawWebhook) -> Result<AppWorkflowJobEvent, WebhookError> {
    if !matches!(raw.action.as_str(), "queued" | "in_progress" | "completed") {
        return Err(WebhookError::UnsupportedAction(raw.action));
    }
    if raw.workflow_job.id == 0 || raw.workflow_job.run_id == 0 {
        return Err(WebhookError::MalformedPayload);
    }
    let installation_id = raw.installation.map(|i| i.id).filter(|&id| id > 0);
    let organization_id = raw.organization.map(|o| o.id).filter(|&id| id > 0);
    Ok(AppWorkflowJobEvent {
        installation_id,
        repository_id: raw.repository.id,
        organization_id,
        job: WorkflowJobEvent {
            action: raw.action,
            repository: raw.repository.full_name,
            job_id: raw.workflow_job.id,
            run_id: raw.workflow_job.run_id,
            run_attempt: raw.workflow_job.run_attempt.max(1),
            head_sha: raw.workflow_job.head_sha,
            labels: raw.workflow_job.labels,
            conclusion: raw.workflow_job.conclusion,
            runner_name: raw.workflow_job.runner_name,
            runner_id: raw.workflow_job.runner_id,
        },
    })
}

fn map_repos(repos: Vec<RawRepo>) -> Vec<InstallationRepo> {
    repos
        .into_iter()
        .map(|r| InstallationRepo {
            id: r.id,
            full_name: r.full_name,
        })
        .collect()
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
    verify_webhook_signature(secret, signature_header, body)?;

    if event_header != "workflow_job" {
        return Err(WebhookError::IgnoredEvent(event_header.to_owned()));
    }

    let raw: RawWebhook =
        serde_json::from_slice(body).map_err(|_| WebhookError::MalformedPayload)?;
    if !scope.allows_repository(&raw.repository.full_name) {
        return Err(WebhookError::RepositoryMismatch(raw.repository.full_name));
    }
    Ok(parse_workflow_job_raw(raw)?.job)
}

/// Verifies `X-Hub-Signature-256` and parses multi-tenant GitHub App webhooks
/// (`workflow_job`, `installation`, and `installation_repositories`) without
/// binding to a single static repository or organization scope.
pub fn verify_and_parse_app_webhook(
    secret: &SecretString,
    signature_header: &str,
    event_header: &str,
    body: &[u8],
) -> Result<GitHubAppWebhookEvent, WebhookError> {
    verify_webhook_signature(secret, signature_header, body)?;

    match event_header {
        "workflow_job" => {
            let raw: RawWebhook =
                serde_json::from_slice(body).map_err(|_| WebhookError::MalformedPayload)?;
            Ok(GitHubAppWebhookEvent::WorkflowJob(parse_workflow_job_raw(
                raw,
            )?))
        }
        "installation" => {
            let raw: RawInstallationWebhook =
                serde_json::from_slice(body).map_err(|_| WebhookError::MalformedPayload)?;
            if !matches!(
                raw.action.as_str(),
                "created" | "deleted" | "suspend" | "unsuspend" | "new_permissions_accepted"
            ) {
                return Err(WebhookError::UnsupportedAction(raw.action));
            }
            if raw.installation.id == 0 || raw.installation.account.id == 0 {
                return Err(WebhookError::MalformedPayload);
            }
            Ok(GitHubAppWebhookEvent::Installation(InstallationEvent {
                action: raw.action,
                installation_id: raw.installation.id,
                account_login: raw.installation.account.login,
                account_id: raw.installation.account.id,
                account_type: raw.installation.account.account_type,
                repository_selection: raw.installation.repository_selection,
                repositories: map_repos(raw.repositories),
            }))
        }
        "installation_repositories" => {
            let raw: RawInstallationReposWebhook =
                serde_json::from_slice(body).map_err(|_| WebhookError::MalformedPayload)?;
            if !matches!(raw.action.as_str(), "added" | "removed") {
                return Err(WebhookError::UnsupportedAction(raw.action));
            }
            if raw.installation.id == 0 {
                return Err(WebhookError::MalformedPayload);
            }
            Ok(GitHubAppWebhookEvent::InstallationRepositories(
                InstallationRepositoriesEvent {
                    action: raw.action,
                    installation_id: raw.installation.id,
                    repository_selection: raw.repository_selection,
                    repositories_added: map_repos(raw.repositories_added),
                    repositories_removed: map_repos(raw.repositories_removed),
                },
            ))
        }
        other => Err(WebhookError::IgnoredEvent(other.to_owned())),
    }
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

    #[test]
    fn parses_optional_runner_and_conclusion_fields_for_in_progress_and_completed() {
        let secret = SecretString::new("0123456789abcdef0123456789abcdef");
        let scope = GitHubScope::Repository("acme/app".to_owned());

        let queued = br#"{"action":"queued","repository":{"full_name":"acme/app"},"workflow_job":{"id":42,"run_id":7,"labels":["self-hosted","open-ci-api"]}}"#;
        let q_sig = sign_webhook_body(&secret, queued).unwrap_or_default();
        let q_ev = verify_and_parse_webhook(&secret, &q_sig, "workflow_job", queued, &scope);
        assert!(matches!(
            q_ev,
            Ok(ev) if ev.conclusion.is_none() && ev.runner_name.is_none() && ev.runner_id.is_none()
        ));

        let in_progress = br#"{"action":"in_progress","repository":{"full_name":"acme/app"},"workflow_job":{"id":42,"run_id":7,"labels":["self-hosted","open-ci-api"],"runner_name":"open-ci-api-abc","runner_id":9001}}"#;
        let ip_sig = sign_webhook_body(&secret, in_progress).unwrap_or_default();
        let ip_ev = verify_and_parse_webhook(&secret, &ip_sig, "workflow_job", in_progress, &scope);
        assert!(matches!(
            ip_ev,
            Ok(ev) if ev.action == "in_progress"
                && ev.conclusion.is_none()
                && ev.runner_name.as_deref() == Some("open-ci-api-abc")
                && ev.runner_id == Some(9001)
        ));

        let completed = br#"{"action":"completed","repository":{"full_name":"acme/app"},"workflow_job":{"id":42,"run_id":7,"labels":["self-hosted","open-ci-api"],"conclusion":"success","runner_name":"open-ci-api-abc","runner_id":9001}}"#;
        let c_sig = sign_webhook_body(&secret, completed).unwrap_or_default();
        let c_ev = verify_and_parse_webhook(&secret, &c_sig, "workflow_job", completed, &scope);
        assert!(matches!(
            c_ev,
            Ok(ev) if ev.action == "completed"
                && ev.conclusion.as_deref() == Some("success")
                && ev.runner_name.as_deref() == Some("open-ci-api-abc")
                && ev.runner_id == Some(9001)
        ));
    }
}
