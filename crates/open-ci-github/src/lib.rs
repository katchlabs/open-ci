//! Typed GitHub REST API client, HMAC webhook verifier, OIDC JWKS verifier,
//! and privilege-separated GitHub App credential broker.

pub mod app_auth;
pub mod broker;
pub mod client;
pub mod jwks;
pub mod oidc;
pub mod runtime_token;
#[cfg(feature = "test-support")]
pub mod test_issuer;
pub mod webhook;

pub use app_auth::{AppJwtSigner, GitHubAppTokenMinter, RsaPemJwtSigner};
pub use client::{
    FileGitHubToken, GitHubClient, GitHubError, GitHubJob, GitHubRunner, GitHubTokenSource,
    GitHubWorkflowRun, StaticGitHubToken,
};
pub use oidc::{
    OidcClaims, OidcError, OidcVerifier, VerifiedOidcToken, validate_oidc_policy,
    verify_oidc_signature,
};
pub use runtime_token::{
    CacheScopeGrant, RuntimeIdentity, RuntimeTokenError, RuntimeTokenPolicy, RuntimeTokenVerifier,
};
pub use webhook::{
    AppWorkflowJobEvent, GitHubAppWebhookEvent, InstallationEvent, InstallationRepo,
    InstallationRepositoriesEvent, WebhookError, WorkflowJobEvent, verify_and_parse_app_webhook,
    verify_and_parse_webhook, verify_webhook_signature,
};
