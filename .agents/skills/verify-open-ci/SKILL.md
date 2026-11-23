---
name: verify-open-ci
description: Empirical runtime verification skill for open-ci. Runs the reusable drive.sh harness (Launch -> Doctor -> Drive -> HTTP + SQLite DB Evidence -> Cleanup) against the real compiled open-ci binary, cache server, and actions/route.
---

# Verify `open-ci` Empirically (Reusable Driver + HTTP & DB Evidence)

"It compiles" and mocked tests are not runtime proof. Spend verification tokens running the **real compiled `open-ci` binary**, driving live HTTP/CLI/Twirp/OIDC endpoints, and checking both **resulting HTTP state AND persisted SQLite DB side effects** against [features.md](features.md).

## 1. Use the Reusable Driver First

Do not improvise throwaway curl loops when a deterministic driver already exists:

```bash
# Full 7-gate ladder (runs Gate 4 via scripts/drive.sh automatically)
./scripts/verify.sh

# Run the standalone live binary + SQLite driver directly (faster inner loop)
./.agents/skills/verify-open-ci/scripts/drive.sh

# Reuse already-built target/debug/open-ci binary
./.agents/skills/verify-open-ci/scripts/drive.sh --skip-build
```

`scripts/drive.sh` executes the full **Launch → Doctor → Drive → Evidence → Cleanup** loop:
1. **Launch**: Compiles `target/debug/open-ci`, allocates an ephemeral loopback port, isolates SQLite in `$TMP_DIR`, and installs an `EXIT` cleanup trap.
2. **Doctor**: Polls `GET /healthz` and `GET /readyz` until HTTP `200` (fails loudly with server logs on timeout).
3. **Drive**: Exercises unauthenticated `401` on `/api/v1/overview`, authenticated `200` (`mode="active"`), Prometheus `/metrics`, live `open-ci ctl set-mode draining`, and `GET /api/v1/audit`.
4. **Evidence (HTTP + SQLite Side Effects)**: Queries the isolated `open-ci-demo.db` directly with `sqlite3` to verify `control_state` (`draining|verify smoke`), persisted `audit_log` rows, and `PRAGMA integrity_check = ok` — catching silent save failures that HTTP-only checks miss.
5. **Cleanup**: Stops the process and deletes `$TMP_DIR`.

## 2. Build the Lever & Stop the Line

When verifying a **new** runtime flow not yet in `scripts/drive.sh`:
1. **Do it by hand once** against the real binary (`open-ci demo`, `open-ci cache`, `open-ci backup`, `open-ci validate`, `actions/route`) and record the exact action, HTTP response, and file/DB side effect.
2. **Build the lever**: If the check will run again, fold it into `scripts/drive.sh` (or a loopback wire test in `prove-invariants`) so future runs spend minimal tokens and get deterministic signal.
3. **Stop the Line**: If Doctor, Drive, or DB side-effect verification fails, stop immediately and diagnose from logs — never ignore a flaky check or move on.

## 3. Targeted Subsystem Checks

When touching areas beyond the default `demo` smoke path:
- **Online SQLite Backup**: Run `target/debug/open-ci backup --config <tmp-cfg> --output "$TMP_DIR/snap.db"` while `serve` holds the exclusive `fs2` lock, then verify `sqlite3 "$TMP_DIR/snap.db" "PRAGMA integrity_check;"`.
- **Transparent Cache (`open-ci cache`)**: Drive `/healthz`, Twirp v2 `CreateCacheEntry` → chunked/single `PUT /v1/cache/blobs/{id}` → `FinalizeCacheEntryUpload` → `GetCacheEntryDownloadURL` → `GET /v1/cache/blobs/{id}`, plus cross-scope denial and blob file / index persistence.
- **Routing Action (`actions/route`)**: Run `node --test actions/route/test/route.test.ts` verifying fail-closed fallback on reruns, timeouts, 4xx, and malformed responses.

## 4. Evidence Format (Proof Artifacts First)

In your summary or PR description, report concrete **Action → HTTP/Runtime State → DB/File Side Effect** facts:
- Exact command(s) executed + exit code `0`.
- Observed status codes and transitions (e.g. `unauth=/api/v1/overview -> 401`, `ctl set-mode draining -> overview=draining`, `sqlite control_state=draining|verify smoke, audit_rows=1, integrity=ok`).
- Never paste fabricated or unexecuted output.

## 5. Maintain This Skill (Keep the Driver Sharp)

Verification harnesses rot if left behind when routes or schemas evolve. Whenever you add or change an HTTP route, CLI subcommand, admission mode, cache endpoint, or SQLite table:
1. Update [features.md](features.md) in the same PR.
2. Extend `.agents/skills/verify-open-ci/scripts/drive.sh` if the new behavior belongs in the live runtime smoke loop.
3. Confirm `bash -n .agents/skills/verify-open-ci/scripts/drive.sh` and `./scripts/verify.sh` both exit `0`.
