# Security Policy

## Threat Model & Guarantees

`open-ci` is designed around defense-in-depth for ephemeral CI runners:

- **Zero Cloud IAM on Runners**: Ephemeral runner VMs are provisioned with `serviceAccounts: []` on GCP and no IAM instance profile on AWS, with IMDSv2 (`HttpTokens=required`) enforced on EC2.
- **Zero Inbound Network Access**: Runner subnets deny all inbound traffic; runners initiate outbound HTTPS only to GitHub and the `open-ci` bootstrap/usage endpoints.
- **Single-Use Bootstrap Credentials**: Each worker receives a per-worker HMAC-SHA256 bearer token derived from a controller secret. Exchange for a GitHub JIT runner config is enforced atomically in SQLite (`bootstrap_used = 0 -> 1`).
- **Privilege-Separated Credential Rotation**: GitHub App private keys are held exclusively by `open-ci rotate-credentials` running under a separate OS user, writing short-lived installation tokens atomically with restricted permissions.
- **Bounded Lifetimes**: Hard worker lifetimes are enforced independently by the control plane reconciler, cloud-native termination (`maxRunDuration` on GCP / `open-ci-aws-janitor` on AWS), and guest-level shutdown.
- **Fail-Closed Workflow Routing**: OIDC admission validates repository ID, owner ID, audience, subject, and pinned workflow SHA. Any error or capacity exhaustion falls back to user-configured fallback runners.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report vulnerabilities privately through **GitHub Security Advisories** (`Security` -> `Report a vulnerability` on this repository). Include:

1. A clear description of the affected crate, endpoint, or IaC module.
2. Reproduction steps or a minimal proof-of-concept.
3. Expected security impact.

We aim to acknowledge reports within 48 hours and coordinate a fix before public disclosure.
