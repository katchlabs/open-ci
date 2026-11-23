# `open-ci` Behavior-Level Feature Map

Every public route, operator endpoint, admission mode, CLI command, routing action contract, and cache endpoint that runtime proof must preserve.

## 1. Public Machine Ingress (`crates/open-ci-server/src/http/ingress.rs`)

| Route | Auth & Limits | Expected Behavior |
| --- | --- | --- |
| `GET /healthz` | Public | Returns HTTP `200` (`ok`) when process is alive. |
| `GET /readyz` | Public | Checks live SQLite connectivity; returns `200` `{"status":"ready"}` or `503` if store is unavailable. |
| `POST /v1/ingress/webhook` | Constant-time HMAC-SHA256 (`X-Hub-Signature-256`) | Rejects invalid/missing signature (`401`). Records `workflow_job` state (`queued`/`in_progress`/`completed`) and triggers reconciliation. |
| `POST /v1/ingress/admit` | GitHub Actions OIDC JWT (`Authorization: Bearer`) | Verifies issuer/JWKS, audience, repo/org ID, subject/workflow allowlists. Returns scoped admission decision (`admitted` + pool runner label, or fail-closed fallback). |
| `POST /v1/workers/{name}/bootstrap` | Single-use HMAC bootstrap token | Consumes token atomically (`UPDATE ... WHERE bootstrap_used = 0`). Returns GitHub JIT runner config once (`200`); replay or expired token returns `409`/`401`. |
| `POST /v1/workers/{name}/usage` | Worker HMAC token + 512-byte body cap | Records worker network telemetry; rejects oversized bodies (`413`) or invalid auth (`401`). |

## 2. Operator API (`crates/open-ci-server/src/http/operator.rs`)

All operator routes require `Authorization: Bearer <operator-token>` (unauthenticated requests MUST return HTTP `401`).

| Route | Expected Behavior |
| --- | --- |
| `GET /api/v1/overview` | Returns control `mode`, capacity envelope, reserved capacity, active worker counts, held admissions, active backoffs. |
| `GET /api/v1/pools` | Lists configured runner pools and placements. |
| `GET /api/v1/workers` | Lists active and historical workers with lifecycle phase and placement metadata. |
| `GET /api/v1/jobs` | Lists observed GitHub `workflow_job` records and routing state. |
| `GET /api/v1/admissions` | Lists active and historical run admissions. |
| `GET /api/v1/backoffs` | Lists active regional quota/capacity backoffs. |
| `GET /api/v1/audit` | Lists operator mutation audit log entries. |
| `POST /api/v1/control/mode` | Persists control mode transitions (`active`, `fallback-only`, `draining`, `paused`) and writes an audit record. |
| `POST /api/v1/reconcile` | Triggers an immediate non-blocking reconciliation pass. |
| `GET /api/v1/events` | Streams live Server-Sent Events (SSE) for real-time operator dashboards. |
| `GET /metrics` | Returns Prometheus text exposition metrics (`open_ci_*`). |

## 3. Admission Modes (`open-ci-core`)

| Mode | Behavior |
| --- | --- |
| `webhook` (direct) | Provisions ephemeral runners directly from `workflow_job` webhooks without requiring `actions/route` OIDC pre-admission. |
| `oidc` | Requires prior OIDC admission via `/v1/ingress/admit` before provisioning; un-admitted or rerun jobs fall back. |
| `hybrid` | Supports both OIDC-admitted runs (with capacity reservation & fail-closed routing) and direct webhook-driven pools. |

## 4. Transparent Cache Server (`crates/open-ci-cache/src/http.rs`)

| Route | Auth & Limits | Expected Behavior |
| --- | --- | --- |
| `GET /healthz` | Public | Returns HTTP `200` (`ok`). |
| `POST /twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry` | Scoped HMAC bearer token, Twirp body cap | Reserves cache key+version within token scope; returns signed upload URL. |
| `POST /twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload` | Scoped HMAC bearer token, Twirp body cap | Commits uploaded blob into SQLite index; idempotent within scope. |
| `POST /twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL` | Scoped HMAC bearer token, Twirp body cap | Resolves exact/prefix key strictly within caller scope; returns signed download URL or miss. |
| `PUT` / `GET` / `HEAD /v1/cache/blobs/{id}` | HMAC-signed URL query params (streaming body) | Streams blob bytes to/from `LocalFs` NVMe or SigV4 S3/R2; rejects tampered/expired signatures or path traversal. |

## 5. CLI & Action Entrypoints

- `open-ci validate --config <file>`: Offline schema/invariant validation printing JSON capacity summary.
- `open-ci demo --listen <addr>`: Zero-cloud local sandbox backed by `MockProvider` (`demo-operator-token-000000000000`).
- `open-ci ctl --url <url> --token <tok> {status|set-mode|reconcile}`: Live operator CLI over `/api/v1/*`.
- `open-ci backup --config <file> --output <path>`: Consistent online SQLite backup while `serve` holds writer lock.
- `open-ci cache --config <file>`: Runs standalone transparent v2 cache server.
- `open-ci rotate-credentials --broker-config <file>`: Privilege-separated atomic token rotation (`0o640`).
- `open-ci agent bootstrap`: Guest VM bootstrap consuming one-time JIT config and enforcing guest-level shutdown.
- `actions/route`: Zero-dependency Node 22+ action failing closed to configured fallback labels on reruns, timeouts, 4xx, or malformed responses.
