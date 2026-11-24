//! Typed GitHub REST API client, HMAC webhook verifier, OIDC JWKS verifier,
//! and privilege-separated GitHub App credential broker.

pub mod broker;
pub mod client;
pub mod jwks;
pub mod oidc;
pub mod runtime_token;
#[cfg(feature = "test-support")]
pub mod test_issuer;
pub mod webhook;

pub use client::{
    FileGitHubToken, GitHubClient, GitHubError, GitHubJob, GitHubRunner, GitHubTokenSource,
    GitHubWorkflowRun, StaticGitHubToken,
};
pub use oidc::{OidcClaims, OidcError, OidcVerifier};
pub use runtime_token::{
    CacheScopeGrant, RuntimeIdentity, RuntimeTokenError, RuntimeTokenPolicy, RuntimeTokenVerifier,
};
pub use webhook::{WebhookError, WorkflowJobEvent, verify_and_parse_webhook};
