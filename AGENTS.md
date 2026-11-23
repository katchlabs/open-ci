# AGENTS.md — `open-ci` Engineering Operating Manual

`open-ci` is a multi-cloud ephemeral GitHub Actions runner control plane and transparent v2 cache server written in Rust (edition 2024, MSRV 1.89) with a zero-dependency TypeScript admission action and Terraform/Packer IaC.

We build **loops you can trust** — not blind trust and not ceremony. Invest effort in clean codebase architecture, deterministic lints/types, and real empirical verification so agents and humans can ship rapidly without slop.

## Workspace Boundaries

Folder and crate structure is context — keep each boundary strict:

| Component | Path | Responsibility |
| --- | --- | --- |
| `open-ci-core` | `crates/open-ci-core` | Pure domain state machine, config validation, admission/reconcile/backoff rules, `proptest` invariants. **Zero I/O.** |
| `open-ci-store` | `crates/open-ci-store` | Durable SQLite (`rusqlite` WAL) state, migrations, `fs2` exclusive writer lock, online backup, atomic bootstrap consumption. |
| `open-ci-provider` | `crates/open-ci-provider` | `CloudProvider` trait and normalized cloud VM/quota error types. |
| `open-ci-provider-aws` | `crates/open-ci-provider-aws` | Native Rust EC2 Query API, SigV4 signing, XML parsing, IMDSv2 & STS `AssumeRole` (BYOC `ExternalId`). |
| `open-ci-provider-gcp` | `crates/open-ci-provider-gcp` | Native Rust Compute Engine REST adapter, metadata server & IAM `generateAccessToken` impersonation. |
| `open-ci-provider-mock` | `crates/open-ci-provider-mock` | In-memory `CloudProvider` for deterministic E2E tests and `open-ci demo`. |
| `open-ci-github` | `crates/open-ci-github` | Constant-time HMAC-SHA256 webhooks, OIDC JWKS verification, GitHub REST/JIT APIs, credential-rotation broker. |
| `open-ci-server` | `crates/open-ci-server` | Axum ingress/operator HTTP router, reconciler loop, SSE event fan-out, Prometheus `/metrics`. |
| `open-ci-cache` | `crates/open-ci-cache` | GitHub Actions Cache v2 Twirp server + signed blob endpoints (`LocalFs` NVMe and SigV4 S3/R2 backends). |
| `open-ci-aws-janitor` | `crates/open-ci-aws-janitor` | Standalone Rust AWS Lambda enforcing cloud-side hard VM expiry on tagged EC2 instances. |
| `open-ci-cli` | `crates/open-ci-cli` | Thin CLI binary (`serve`, `cache`, `demo`, `validate`, `ctl`, `backup`, `rotate-credentials`, `agent bootstrap`). |
| `actions/route` | `actions/route` | Zero-dependency Node 22+ TypeScript GitHub Action calling `/v1/ingress/admit` with fail-closed fallback. |
| `infra/` | `infra/terraform`, `infra/packer` | Cloud foundation/controller Terraform modules & examples + Packer Ubuntu runner image & scripts. |

## Hard Invariants

1. **Hexagonal Purity**: `open-ci-core` MUST have zero `tokio`, `reqwest`, `rusqlite`, `axum`, or `hyper` dependencies.
2. **Non-Blocking Control Plane**: `open-ci-server` MUST NEVER hold a mutex/lock across `.await`; all SQLite work runs inside `tokio::task::spawn_blocking`.
3. **Native Cloud Wire Protocols**: Cloud and GitHub adapters use native Rust HTTP/SigV4/XML/JSON; never shell out to `aws`, `gcloud`, or `gh`.
4. **Fail-Closed Action**: `actions/route` has zero runtime npm dependencies and ALWAYS falls back cleanly on reruns, timeouts, 4xx, or malformed responses.
5. **Defense-in-Depth Ephemeral Runners**: Zero cloud IAM on runners (`"serviceAccounts": []` on GCP; no instance profile on AWS), IMDSv2 enforced (`http_tokens = "required"` and `MetadataOptions.HttpTokens = required`), triple-layer VM expiry, single-use atomic bootstrap (`WHERE bootstrap_used = 0`).
6. **Zero Warning / Zero Bypass Floor**: `unsafe_code = "forbid"`, no `unwrap`/`expect`/`panic`/`todo`/`unimplemented`, and ZERO `#[allow(` attributes in `crates/`.

## The Pyramid of Trust (Fix at the Highest Layer)

1. **Codebase (API & Data Structures)** — Make invalid states unrepresentable and ban footguns. Your codebase is agent memory: every shortcut left in the repo gets copied.
2. **Static Analysis (Lints, Types, CI)** — Deterministic, zero ongoing tokens (`[workspace.lints]`, Clippy `-D warnings`, zero `#[allow(`, hexagonal purity, policy checks). Lint/check failures should tell the agent how to fix the root cause.
3. **Verification (Empirical, Property, Formal)** — Spend verification tokens actually running the compiled binary (`drive.sh` checking HTTP + SQLite DB side effects), `proptest` randomized invariants, real SQLite concurrency races, and loopback wire contracts — not agents arguing over diffs.
4. **Prose (`AGENTS.md`, skills)** — Minimal; only what cannot be checked mechanically.
5. **Human Review** — Inspect empirical proof artifacts first, then skim the diff. Relying on humans to catch recurring mistakes does not scale.

## Operating Rules

- **Always**: Start with a **Goal + Check** ("repro first, then fix and verify") — no planning theater. Fix root causes, never band-aid quick fixes. Run `./scripts/verify.sh` before claiming done. Stop the line immediately when any gate fails. Keep inline comments sparse (`no-comments`); turn claimed invariants into types, lints, or tests. Prove the inner loop locally before parallelizing in `git worktree`s.
- **Ask First**: Changing public HTTP routes or schemas, adding workspace dependencies, modifying IAM/OIDC/HMAC trust boundaries, or changing SQLite migrations.
- **Never**: Weaken a check to pass; add `#[allow(...)]` to silence Clippy; ship mocked-only proof for runtime/wire changes; waste tokens on adversarial diff-arguing loops instead of running empirical/property verification; commit secrets or `.audit/` scratch logs.

## Canonical Verification

```bash
./scripts/verify.sh                                # Full 7-gate proof ladder (runs live binary + SQLite driver)
./scripts/verify.sh --fast                         # Fast static, tests, config, route action, policy & doc parity
./.agents/skills/verify-open-ci/scripts/drive.sh   # Standalone compiled-binary + HTTP + SQLite side-effect driver
```

## Skills (`.agents/skills/`)

| Skill | Purpose |
| --- | --- |
| [`open-ci-workflow`](.agents/skills/open-ci-workflow/SKILL.md) | Goal + Check routing (`feature`, `bug-fix`, `refactor`, `autonomous-run`), root-cause discipline, stop-the-line, worktrees. |
| [`verify-open-ci`](.agents/skills/verify-open-ci/SKILL.md) | Empirical runtime proof via reusable [`scripts/drive.sh`](.agents/skills/verify-open-ci/scripts/drive.sh) (HTTP + SQLite DB evidence) + [Feature Map](.agents/skills/verify-open-ci/features.md). |
| [`prove-invariants`](.agents/skills/prove-invariants/SKILL.md) | `proptest` domain invariants, randomized state checks, real SQLite concurrency races, loopback Axum wire contracts. |
| [`encode-correction`](.agents/skills/encode-correction/SKILL.md) | Push recurring mistakes up the Pyramid (Codebase → Static Analysis → Verification → Prose), proven red-before-green. |
| [`no-comments`](.agents/skills/no-comments/SKILL.md) | "Your Codebase Is Memory" hygiene: strip narrating/excuse inline comments, ban footguns, turn prose rules into types/lints. |
| [`ship-pr`](.agents/skills/ship-pr/SKILL.md) | Proof-artifacts-first PRs, Conventional Commits, zero AI slop, concise `Attention` callouts. |
