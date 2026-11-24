# open-ci

**Self-hosted ephemeral GitHub Actions runners on Spot & On-Demand AWS and GCP VMs — written in Rust, with transparent Actions v2 caching, auto-NVMe scratch storage, and zero cloud IAM on workers.**

[![CI](https://github.com/katchlabs/open-ci/actions/workflows/ci.yml/badge.svg)](https://github.com/katchlabs/open-ci/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](rust-toolchain.toml)

---

```mermaid
flowchart LR
    GH["GitHub Actions"] -->|"workflow_job webhook or OIDC JWT"| CP["open-ci Control Plane (Rust / Axum / SQLite)"]
    CP -->|"EC2 SigV4 / Compute Engine REST"| VM["Ephemeral Spot / On-Demand VMs (Zero IAM, Auto-NVMe)"]
    VM -->|"Single-use HMAC bootstrap -> JIT config"| CP
    VM -->|"Actions Cache v2 (Twirp + Blob)"| CACHE["open-ci cache (Local NVMe, S3 or GCS)"]
    VM -->|"Outbound HTTPS only"| GH
    OP["Operator CLI / Prometheus / SSE"] -->|"Private loopback or VPN + Bearer"| CP
```

## Why open-ci?

- **Drop-in or fail-closed routing.** Use `admission.mode = "webhook"` for zero workflow edits (`runs-on: open-ci-linux-x64`), `admission.mode = "oidc"` with [`actions/route`](actions/route) to fail closed to `ubuntu-latest` on outage, drain, or capacity exhaustion, or `"hybrid"` for both.
- **Org-wide and multi-repo native.** Target a single `owner/repo` or an entire GitHub organization with `allowed_repositories` allowlists (e.g. `["my-org/*"]`) and `runner_group_id` JIT registration.
- **Multi-cloud without SDK bloat.** Pure-Rust AWS EC2 + STS SigV4/XML and GCP Compute Engine REST adapters — no `aws` or `gcloud` CLI subprocesses, with native BYOC identity (`IMDSv2` + STS `AssumeRole` with `ExternalId`; GCE metadata + IAM service account impersonation).
- **Fast builds out of the box.** Built-in GitHub Actions v2 transparent cache (`open-ci cache`) that authorizes each job with its own runtime token and isolates entries per repository and branch exactly like GitHub, backed by local NVMe, native S3 (S3/R2/MinIO), or native GCS with keyless VM credentials, plus boot-time auto-detection and `ext4`/RAID0 mounting of local ephemeral NVMe SSDs for Docker layers, `RUNNER_WORKDIR`, and `TMPDIR`.
- **Zero-IAM, hard-bounded runners.** Workers boot with zero cloud IAM (`serviceAccounts: []` on GCP; no instance profile + IMDSv2 required on AWS), zero inbound ports, single-use atomic HMAC bootstrap tokens, and triple-layer expiry (reconciler + `maxRunDuration`/`open-ci-aws-janitor` + guest `poweroff`).
- **Single binary, strict Rust safety.** Compiles to static-friendly `x86_64` and `aarch64` Linux binaries and multi-arch GHCR containers under `unsafe_code = "forbid"`, `clippy::unwrap_used = "deny"`, `expect_used = "deny"`, and `panic = "deny"`.

---

## 60-Second Local Quickstart (Zero Cloud Accounts)

Run the full control plane locally against `MockProvider` to inspect the operator API, SSE event stream, and Prometheus metrics before touching AWS or GCP.

### 1. Install or run `open-ci`

```bash
# Option A: Multi-arch container (linux/amd64, linux/arm64)
docker run --rm -p 8080:8080 ghcr.io/katchlabs/open-ci:latest demo --listen 0.0.0.0:8080

# Option B: Prebuilt Linux release binary (x86_64 or aarch64)
curl -fsSL https://github.com/katchlabs/open-ci/releases/latest/download/open-ci-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz -C /usr/local/bin
open-ci demo --listen 127.0.0.1:8080

# Option C: From source (Rust 1.89+)
cargo run --bin open-ci -- demo --listen 127.0.0.1:8080
```

### 2. Query the live control plane

In a second terminal:

```bash
export OPEN_CI_URL="http://127.0.0.1:8080"
export OPEN_CI_OPERATOR_TOKEN="demo-operator-token-000000000000"

open-ci ctl status
```

Example output:

```json
{
  "active_backoffs": 0,
  "admission_mode": "hybrid",
  "capacity": {
    "held_admissions": 0,
    "limit": {
      "disk_gb": 300,
      "instances": 8,
      "vcpu": 16
    },
    "reserved": {
      "disk_gb": 20,
      "instances": 1,
      "vcpu": 2
    }
  },
  "control": {
    "mode": "active",
    "reason": "initial",
    "updated_at": 0,
    "updated_by": "system"
  },
  "organization": null,
  "pools_configured": 1,
  "repository": "example/repo",
  "scope": "example/repo",
  "timestamp": 1763916000,
  "workers": {
    "active": 0,
    "by_phase": {},
    "by_provider": {}
  }
}
```

Inspect Prometheus metrics, stream live SSE events, or drain the control plane:

```bash
curl -sS -H "Authorization: Bearer $OPEN_CI_OPERATOR_TOKEN" "$OPEN_CI_URL/metrics"
curl -sS -N -H "Authorization: Bearer $OPEN_CI_OPERATOR_TOKEN" "$OPEN_CI_URL/api/v1/events"
open-ci ctl set-mode draining --reason "maintenance"
open-ci ctl set-mode active --reason "resume"
```

Validate any configuration file offline before deploying:

```bash
open-ci validate --config config/open-ci.example.toml
```

---

## 2-Minute Workflow Wiring

Choose the admission mode that fits your repository or organization.

### Option A: Direct Webhook Mode (Zero Workflow Changes)

Set `admission.mode = "webhook"` in `open-ci.toml`. Point a GitHub `workflow_job` webhook at `https://<your-domain>/v1/ingress/webhook`, then use your pool's `runner_label` directly:

```yaml
jobs:
  build:
    runs-on: open-ci-linux-x64
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace
```

When GitHub emits `workflow_job` (`queued`), `open-ci` verifies the HMAC-SHA256 signature, checks capacity guardrails, and launches a Spot or On-Demand VM that registers a single-use JIT runner for that label.

### Option B: Fail-Closed OIDC Routing (`actions/route`)

Set `admission.mode = "oidc"` (or `"hybrid"` to support both). A lightweight routing job exchanges a GitHub OIDC JWT at `/v1/ingress/admit` for a scoped reservation label. If `open-ci` is unreachable, draining, at capacity, or handling a rerun (`fallback_on_rerun = true`), `actions/route` falls back cleanly to your configured runner label (`ubuntu-latest` or any custom label):

```yaml
jobs:
  route:
    runs-on: ubuntu-latest
    permissions:
      id-token: write
      contents: read
    outputs:
      runners: ${{ steps.route.outputs.runners }}
    steps:
      - uses: actions/checkout@v4
      - id: route
        uses: ./actions/route
        with:
          endpoint: https://ci.example.com/v1/ingress/admit
          audience: open-ci:example-org/example-repo
          pools: '["default"]'
          fallbacks: '{"default": "ubuntu-latest"}'

  build:
    needs: route
    runs-on: ${{ fromJSON(needs.route.outputs.runners).default }}
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace
```

---

## Workspace & Infrastructure Map

| Path | Responsibility |
| --- | --- |
| `crates/open-ci-core` | Zero-I/O domain core: typed TOML config + validation, worker state machine, admission capacity math, placement selection, regional quota backoff, `SecretString`, `Clock`. |
| `crates/open-ci-store` | Async `Store` trait and non-blocking normalized `SQLite` backend (`spawn_blocking`, WAL, `0o600` pre-creation, `fs2` exclusive lockfile, online backup snapshots). |
| `crates/open-ci-provider` | Cloud-agnostic `CloudProvider` trait, typed `ProviderError`, `InstanceSpec`, and `ProviderRegistry`. |
| `crates/open-ci-provider-aws` | Pure-Rust AWS EC2 + STS adapter (SigV4 signing, XML parsing, Spot/On-Demand, IMDSv2, cross-account `AssumeRole` with `ExternalId`). |
| `crates/open-ci-provider-gcp` | Native Rust GCP Compute Engine REST adapter (Spot/On-Demand, `maxRunDuration`, `serviceAccounts: []`, GCE metadata & IAM impersonation). |
| `crates/open-ci-provider-mock` | Deterministic fault-injecting provider for tests and `open-ci demo`. |
| `crates/open-ci-github` | Typed GitHub REST client (bounded pagination, repo & org JIT registration), constant-time HMAC webhook verifier, RS256 OIDC JWT verifier, privilege-separated credential broker. |
| `crates/open-ci-server` | Non-blocking `ControlPlane`, background reconciler, public machine ingress, authenticated Operator API, SSE stream, and Prometheus `/metrics`. |
| `crates/open-ci-cache` | Native GitHub Actions v2 transparent cache server (Twirp RPCs + Azure block blob protocol): runtime-token auth with ref-scoped entries, `SQLite` index with retention, streaming `local`/`s3`/`gcs` blob stores, and results-service passthrough. |
| `crates/open-ci-aws-janitor` | Standalone Rust CLI and AWS Lambda (`provided.al2023`) expiry guard that terminates leaked EC2 instances and detached EBS volumes with per-resource fault isolation. |
| `crates/open-ci-cli` | Unified CLI binary (`serve`, `cache`, `demo`, `validate`, `ctl`, `backup`, `rotate-credentials`, `agent bootstrap`). |
| `actions/route` | Zero-dependency TypeScript (`node24`) GitHub Action for fail-closed OIDC admission routing. |
| `infra/terraform` | Turnkey modules (`gcp-foundation`, `gcp-controller`, `aws-foundation`, `aws-controller`) and examples (`gcp-quickstart`, `aws-controller`, `multi-cloud`). |
| `infra/packer` | Unified GCP + AWS Packer template (`ubuntu-runner.pkr.hcl`) for `x64`/`arm64` Ubuntu 24.04 runners with boot-time auto-NVMe (`setup-nvme.sh`), Docker BuildKit, and sysctl tuning. |
| `deploy/` | Hardened systemd units (`open-ci.service`, `open-ci-cache.service`, `open-ci-credentials.timer`, `open-ci-backup.timer`), Caddy reverse proxy template, and multi-stage Dockerfile. |

---

## Transparent Tradeoffs & Non-Goals

We prefer clear engineering boundaries over marketing claims:

- **Single-node SQLite control plane today.** `open-ci-store` uses SQLite in WAL mode with an exclusive `fs2` lockfile and hourly online backups. One small controller VM (`t4g.small` / `e2-small`) comfortably handles hundreds of concurrent runners, and fail-closed `actions/route` handles controller downtime without breaking CI — but active-active multi-node HA requires the planned `PostgresStore`.
- **Linux runners only today.** Packer templates and `open-ci agent bootstrap` target `x86_64` and `aarch64` Ubuntu Linux VMs. macOS and Windows runners are out of scope for now.
- **When GitHub-hosted runners are simpler.** If your CI bill is low, your builds fit comfortably on standard 4-core runners, and you do not need private VPC access, large Spot instances, NVMe scratch disks, or self-hosted Actions v2 caching, standard GitHub-hosted runners have zero operational overhead.
- **Not a generic job orchestrator.** `open-ci` manages ephemeral VM fleets for GitHub Actions. It does not replace Kubernetes, Nomad, or general batch schedulers.

---

## Roadmap

| Status | Milestone | Notes |
| :---: | --- | --- |
| ✓ | Multi-cloud AWS + GCP Spot & On-Demand provisioning | Pure-Rust SigV4/XML EC2 + Compute Engine REST |
| ✓ | Org-level & multi-repo runner pools | `organization`, `allowed_repositories`, `runner_group_id` |
| ✓ | `webhook`, `oidc`, and `hybrid` admission modes | Drop-in webhooks or fail-closed `actions/route` |
| ✓ | Native GitHub Actions v2 transparent cache (`open-ci cache`) | Local NVMe, native S3 / R2 / MinIO, and native GCS; verified with the official `@actions/cache` client |
| ✓ | Auto-NVMe RAID0/ext4 boot setup & BuildKit tuning | `x64` and `arm64` Ubuntu 24.04 Packer images |
| ✓ | BYOC cross-account AWS `AssumeRole` & GCP SA impersonation | `ExternalId` support, EC2 IMDSv2, GCE metadata |
| ✓ | Turnkey Terraform modules & automated multi-arch releases | `aws-controller`, `gcp-controller`, GHCR + `.tar.gz` |
| ○ | `PostgresStore` implementation for active-active HA | Same async `Store` trait; zero domain changes |
| ○ | Operator Web UI | Backed by existing `/api/v1/*` REST + `/api/v1/events` SSE |
| ○ | Additional cloud providers (Hetzner, Azure, Kubernetes) | Via `open-ci-provider` `CloudProvider` trait |
| ○ | Interactive `open-ci init` setup wizard | Generates validated `open-ci.toml` and trust policies |

---

## Documentation

- [**Deployment Quickstart**](docs/quickstart.md) — End-to-end AWS-only, GCP-only, or multi-cloud deployment with Terraform, Packer, systemd, and Caddy.
- [**Architecture Guide**](docs/architecture.md) — Hexagonal crate layout, concurrency model, SQLite durability, multi-cloud scheduling, cache/NVMe pipeline, and exact HTTP API specification.
- [**Security & Threat Model**](docs/security.md) — Zero-IAM runners, atomic single-use bootstrap, OIDC pinning, credential broker isolation, triple-layer expiry, and BYOC trust boundaries.
- [**Engineering & Trust Loop**](docs/engineering.md) — How `open-ci` is engineered and verified: the Pyramid of Trust, `.agents/skills/`, reusable runtime + SQLite driver, and the 7-gate `./scripts/verify.sh` proof ladder.
- [**Contributing**](CONTRIBUTING.md) — Standards, local verification, and PR workflow for human and AI contributors.
- [**Security Policy**](SECURITY.md) — Private vulnerability disclosure process.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
