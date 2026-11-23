#!/usr/bin/env bash
# Reusable empirical runtime driver for open-ci.
# Executes Launch -> Doctor -> Drive -> Evidence (HTTP + SQLite DB side effects) -> Cleanup
# against the real compiled open-ci binary on an isolated loopback port and temp SQLite DB.
#
# Usage:
#   ./.agents/skills/verify-open-ci/scripts/drive.sh [--skip-build]

set -euo pipefail

SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-build)
      SKIP_BUILD=1
      ;;
    -h|--help)
      cat <<'EOF'
Usage: drive.sh [--skip-build]

Drives the real compiled open-ci binary end-to-end and captures structured evidence
of every action, resulting HTTP/runtime state, and SQLite DB persistence side effect:
  1. Launch  : compile target/debug/open-ci (unless --skip-build) on ephemeral loopback port + isolated SQLite DB
  2. Doctor  : poll GET /healthz and GET /readyz until HTTP 200
  3. Drive   : unauthenticated 401, authenticated /api/v1/overview (active), /metrics,
               live CLI mutation (open-ci ctl set-mode draining), /api/v1/audit check,
               and direct SQLite DB verification (control_state + audit_log + PRAGMA integrity_check)
  4. Cleanup : stop background process and delete isolated temp directory on exit
EOF
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $arg (use --skip-build or --help)" >&2
      exit 2
      ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
cd "$REPO_ROOT"

DEMO_TOKEN="demo-operator-token-000000000000"
DEMO_PID=""
SMOKE_TMP_DIR=""

cleanup() {
  if [[ -n "${DEMO_PID}" ]]; then
    kill "${DEMO_PID}" 2>/dev/null || true
    wait "${DEMO_PID}" 2>/dev/null || true
    DEMO_PID=""
  fi
  if [[ -n "${SMOKE_TMP_DIR}" && -d "${SMOKE_TMP_DIR}" ]]; then
    rm -rf "${SMOKE_TMP_DIR}"
    SMOKE_TMP_DIR=""
  fi
}
trap cleanup EXIT INT TERM

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  if [[ -n "${SMOKE_TMP_DIR}" && -f "${SMOKE_TMP_DIR}/demo.log" ]]; then
    echo "--- demo.log ---" >&2
    cat "${SMOKE_TMP_DIR}/demo.log" >&2 || true
  fi
  exit 1
}

# 1. Launch
if [[ "$SKIP_BUILD" -eq 0 ]]; then
  cargo build --quiet --bin open-ci
fi

if [[ ! -x target/debug/open-ci ]]; then
  fail "target/debug/open-ci binary missing; run cargo build --bin open-ci first"
fi

PORT="$(node -e 'const net = require("node:net"); const s = net.createServer(); s.listen(0, "127.0.0.1", () => { console.log(s.address().port); s.close(); });')"
if [[ -z "$PORT" ]]; then
  fail "Failed to allocate an ephemeral loopback port"
fi

SMOKE_TMP_DIR="$(mktemp -d)"
DEMO_LOG="${SMOKE_TMP_DIR}/demo.log"
DB_PATH="${SMOKE_TMP_DIR}/open-ci-demo.db"
BASE_URL="http://127.0.0.1:${PORT}"

# TMPDIR isolates open-ci-demo.db, WAL/SHM, and lock files inside SMOKE_TMP_DIR.
TMPDIR="${SMOKE_TMP_DIR}" target/debug/open-ci demo --listen "127.0.0.1:${PORT}" >"${DEMO_LOG}" 2>&1 &
DEMO_PID="$!"

# 2. Doctor: poll /healthz and /readyz up to 10s (50 x 0.2s)
READY=0
for _ in $(seq 1 50); do
  if ! kill -0 "${DEMO_PID}" 2>/dev/null; then
    fail "open-ci demo exited prematurely during startup"
  fi
  HZ_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE_URL}/healthz" || echo "000")"
  RZ_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE_URL}/readyz" || echo "000")"
  if [[ "$HZ_CODE" == "200" && "$RZ_CODE" == "200" ]]; then
    READY=1
    break
  fi
  sleep 0.2
done
if [[ "$READY" -ne 1 ]]; then
  fail "Timed out waiting for /healthz and /readyz HTTP 200 on ${BASE_URL}"
fi
echo "  ok: doctor passed (/healthz=200, /readyz=200 on ${BASE_URL})"

# 3. Drive 1: unauthenticated operator API must return 401
UNAUTH_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE_URL}/api/v1/overview")"
if [[ "$UNAUTH_CODE" != "401" ]]; then
  fail "Expected HTTP 401 for unauthenticated GET /api/v1/overview, got ${UNAUTH_CODE}"
fi
echo "  ok: action=GET /api/v1/overview (unauth) -> http=401"

# 4. Drive 2: authenticated GET /api/v1/overview -> 200 with mode=active
OVERVIEW_FILE="${SMOKE_TMP_DIR}/overview.json"
OVERVIEW_CODE="$(curl -s -o "${OVERVIEW_FILE}" -w '%{http_code}' \
  -H "Authorization: Bearer ${DEMO_TOKEN}" \
  "${BASE_URL}/api/v1/overview")"
if [[ "$OVERVIEW_CODE" != "200" ]]; then
  cat "${OVERVIEW_FILE}" >&2 || true
  fail "Expected HTTP 200 for authenticated GET /api/v1/overview, got ${OVERVIEW_CODE}"
fi
if ! grep -Eq '"mode"[[:space:]]*:[[:space:]]*"active"' "${OVERVIEW_FILE}"; then
  cat "${OVERVIEW_FILE}" >&2 || true
  fail "Expected GET /api/v1/overview JSON to contain \"mode\":\"active\""
fi
echo "  ok: action=GET /api/v1/overview (auth) -> http=200 mode=active"

# 5. Drive 3: authenticated GET /metrics -> 200 Prometheus text exposition
METRICS_FILE="${SMOKE_TMP_DIR}/metrics.txt"
METRICS_CODE="$(curl -s -o "${METRICS_FILE}" -w '%{http_code}' \
  -H "Authorization: Bearer ${DEMO_TOKEN}" \
  "${BASE_URL}/metrics")"
if [[ "$METRICS_CODE" != "200" ]]; then
  cat "${METRICS_FILE}" >&2 || true
  fail "Expected HTTP 200 for GET /metrics, got ${METRICS_CODE}"
fi
if ! grep -q '# HELP open_ci_' "${METRICS_FILE}" || ! grep -q '# TYPE open_ci_' "${METRICS_FILE}"; then
  cat "${METRICS_FILE}" >&2 || true
  fail "GET /metrics did not return expected Prometheus exposition format"
fi
echo "  ok: action=GET /metrics -> http=200 prometheus=open_ci_*"

# 6. Drive 4: live CLI mutation via open-ci ctl set-mode draining
CTL_OUT="$(target/debug/open-ci ctl \
  --url "${BASE_URL}" \
  --token "${DEMO_TOKEN}" \
  set-mode draining --reason "verify smoke")"
if ! printf '%s\n' "${CTL_OUT}" | grep -Eq '"mode"[[:space:]]*:[[:space:]]*"draining"'; then
  printf '%s\n' "${CTL_OUT}" >&2
  fail "open-ci ctl set-mode draining did not return mode=draining"
fi

OVERVIEW_AFTER="${SMOKE_TMP_DIR}/overview_after.json"
OVERVIEW_AFTER_CODE="$(curl -s -o "${OVERVIEW_AFTER}" -w '%{http_code}' \
  -H "Authorization: Bearer ${DEMO_TOKEN}" \
  "${BASE_URL}/api/v1/overview")"
if [[ "$OVERVIEW_AFTER_CODE" != "200" ]] || ! grep -Eq '"mode"[[:space:]]*:[[:space:]]*"draining"' "${OVERVIEW_AFTER}"; then
  cat "${OVERVIEW_AFTER}" >&2 || true
  fail "GET /api/v1/overview did not reflect mode transition to draining"
fi

AUDIT_FILE="${SMOKE_TMP_DIR}/audit.json"
AUDIT_CODE="$(curl -s -o "${AUDIT_FILE}" -w '%{http_code}' \
  -H "Authorization: Bearer ${DEMO_TOKEN}" \
  "${BASE_URL}/api/v1/audit")"
if [[ "$AUDIT_CODE" != "200" ]] || ! grep -Fq "verify smoke" "${AUDIT_FILE}"; then
  cat "${AUDIT_FILE}" >&2 || true
  fail "GET /api/v1/audit did not record the live set-mode audit entry"
fi
echo "  ok: action=ctl set-mode draining -> http=200 overview=draining audit=recorded"

# 7. Evidence: verify real SQLite DB side effects on disk (catches silent save failures)
if [[ ! -f "${DB_PATH}" ]]; then
  fail "Expected isolated SQLite database file at ${DB_PATH}"
fi

if command -v sqlite3 >/dev/null 2>&1; then
  DB_MODE="$(sqlite3 "${DB_PATH}" "SELECT mode || '|' || reason FROM control_state WHERE id = 1;")"
  if [[ "${DB_MODE}" != "draining|verify smoke" ]]; then
    fail "SQLite control_state mismatch: expected 'draining|verify smoke', got '${DB_MODE}'"
  fi
  AUDIT_ROWS="$(sqlite3 "${DB_PATH}" "SELECT COUNT(*) FROM audit_log WHERE detail_json LIKE '%verify smoke%';")"
  if [[ -z "${AUDIT_ROWS}" || "${AUDIT_ROWS}" -lt 1 ]]; then
    fail "SQLite audit_log missing persisted row for 'verify smoke' (count=${AUDIT_ROWS})"
  fi
  INTEGRITY="$(sqlite3 "${DB_PATH}" "PRAGMA integrity_check;")"
  if [[ "${INTEGRITY}" != "ok" ]]; then
    fail "SQLite PRAGMA integrity_check failed: ${INTEGRITY}"
  fi
  echo "  ok: sqlite db verified (control_state=${DB_MODE}, audit_rows=${AUDIT_ROWS}, integrity=ok)"
else
  echo "  warn: sqlite3 CLI not installed; verified persistence via /api/v1/overview + /api/v1/audit + db file"
fi

# 8. Cleanup immediately (trap also guards error paths)
cleanup
echo "  ok: demo server stopped and temp SQLite directory cleaned"
