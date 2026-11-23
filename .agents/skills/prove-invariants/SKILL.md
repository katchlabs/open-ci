---
name: prove-invariants
description: Property-based, randomized fault-injection, SQLite concurrency, and loopback wire-contract proof skill for open-ci. Zero lazy mocks; invest verification tokens in empirical and formal/property proofs.
---

# Prove Invariants (Property, Fuzzing, Concurrency & Wire Contracts)

Spend verification tokens on **empirical execution, randomized property testing, and formal state checks** — not agents arguing over diffs. Every domain rule, SQLite invariant, and external wire protocol in `open-ci` must be proven with real inputs and real I/O boundaries.

## 1. Pure Domain Property Tests (`crates/open-ci-core`)

`open-ci-core` has zero I/O, making it ideal for fast randomized property testing (`proptest`) in `crates/open-ci-core/tests/invariants.rs` (or unit `proptest!` blocks):

- **Capacity conservation**: Total reserved + active vCPU, instances, and disk NEVER exceed configured `capacity` + `background` bounds across arbitrary sequences of admissions and releases.
- **Admission monotonicity & fail-closed**: `draining`, `paused`, `fallback-only`, reruns (when `fallback_on_rerun = true`), or exhausted capacity NEVER admit onto ephemeral pools.
- **Backoff & placement determinism**: Backed-off regions/zones are skipped until expiry; `prefer_cheapest` / `provider_weights` stay deterministic.
- **Reconciler safety & liveness**: Hard `lifetime_seconds` and `idle_seconds` always emit termination actions; orphan cloud VMs outside live state are reaped; state transitions never strand a worker in an unreachable phase.
- **Config validation**: Invalid configs (zero limits, malformed URLs, unsafe loopback without opt-in, empty pools) fail fast at parse/validation time.

For tricky multi-step concurrency or crash-recovery protocols (e.g., reconciler + lease expiry + atomic bootstrap races), model the state transitions explicitly with a randomized state-machine `proptest` (or TLA+ spec if introducing a new distributed coordination protocol).

Run:
```bash
cargo test -p open-ci-core
```

## 2. Real SQLite Concurrency & Durability Races (`crates/open-ci-store`)

Never mock SQLite. Prove concurrency and crash-safety against real on-disk `rusqlite` WAL databases in `crates/open-ci-store/tests/sqlite_store.rs`:

- **Single-use atomic bootstrap**: Spawn concurrent tasks racing to consume the same worker bootstrap token (`UPDATE ... WHERE bootstrap_used = 0`). Assert **exactly 1** succeeds and all others fail deterministically.
- **Exclusive writer lock (`fs2`)**: Assert a second writer opening the same database path fails immediately rather than corrupting state.
- **Online backup consistency**: Run `SqliteStore::backup_file_online` while live writes occur; open the snapshot and assert schema + row integrity.
- **Idempotent migrations**: Opening an existing database twice preserves data and audit history without error.

Run:
```bash
cargo test -p open-ci-store
```

## 3. Loopback Axum Wire-Contract Proofs (Zero Lazy Mocks)

Every external protocol boundary MUST be tested over real loopback TCP sockets (`127.0.0.1:0`) with an in-process Axum test server validating actual wire bytes:

| Crate | Contract suite | What MUST be asserted on the wire |
| --- | --- | --- |
| `open-ci-provider-aws` | `tests/aws_contract.rs`, `tests/aws_identity_contract.rs` | Real SigV4 `Authorization` + `X-Amz-Date`, EC2 form params (`MetadataOptions.HttpTokens=required`, zero instance profile), real XML parsing, IMDSv2 token PUT/GET, STS `AssumeRole` with `ExternalId`. |
| `open-ci-provider-gcp` | `tests/gcp_contract.rs`, `tests/gcp_identity_contract.rs` | Real REST JSON payload (`"serviceAccounts": []`, `maxRunDuration`), metadata header `Metadata-Flavor: Google`, IAM `generateAccessToken` impersonation. |
| `open-ci-github` | `src/webhook.rs`, `src/oidc.rs`, `src/client.rs`, `src/broker.rs` | Constant-time HMAC-SHA256 verification, JWKS RSA/ECDSA OIDC claim validation (`iss`/`aud`/`sub`/workflow pins), org + repo JIT runner registration, atomic `0o640` credential file rotation. |
| `open-ci-cache` | `tests/cache_service.rs`, `tests/storage_backends.rs` | Real Twirp v2 JSON RPCs, Azure chunked `comp=block` + `comp=blocklist` uploads, scoped token isolation (cross-scope denial), signed blob `PUT`/`GET`/`HEAD`, LocalFs LRU + SigV4 S3/R2 wire requests. |
| `open-ci-server` | `tests/lifecycle_e2e.rs`, `tests/operator_api.rs` | Full webhook / OIDC → admit → scale-up → bootstrap → usage → complete → reap lifecycle, SSE stream, bearer auth, control mode transitions. |

Run:
```bash
cargo test --workspace
```

## 4. Red Flags (Reject Immediately)

- Hand-rolled trait mocks that skip SigV4/XML/JSON/HTTP serialization when testing a provider or GitHub client.
- Tests that sleep arbitrary fixed durations instead of polling or injecting a deterministic `Clock`.
- Suppressing compiler or Clippy checks with `#[allow(...)]` inside tests or production code.
