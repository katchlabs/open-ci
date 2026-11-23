//! Typed GitHub REST API client, HMAC webhook verifier, OIDC JWKS verifier,
//! and privilege-separated GitHub App credential broker.

pub mod broker;
pub mod client;
pub mod oidc;
pub mod webhook;

pub use client::{
    FileGitHubToken, GitHubClient, GitHubError, GitHubJob, GitHubRunner, GitHubTokenSource,
    GitHubWorkflowRun, StaticGitHubToken,
};
pub use oidc::{OidcClaims, OidcError, OidcVerifier};
pub use webhook::{WebhookError, WorkflowJobEvent, verify_and_parse_webhook};
