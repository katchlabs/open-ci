#!/usr/bin/env bash
# Canonical 7-gate executable proof ladder for open-ci.
# Usage:
#   ./scripts/verify.sh          # Full 7-gate proof (includes live compiled binary + SQLite driver)
#   ./scripts/verify.sh --fast   # Fast mode (skips Gate 4 live binary driver)

set -euo pipefail

FAST_MODE=0
for arg in "$@"; do
  case "$arg" in
    --fast)
      FAST_MODE=1
      ;;
    -h|--help)
      cat <<'EOF'
Usage: ./scripts/verify.sh [--fast]

Runs the 7-gate open-ci proof ladder:
  Gate 1: Static, Lint & Hexagonal Purity (fmt, clippy -D warnings, zero #[allow(, open-ci-core purity)
  Gate 2: Domain Invariants, Storage, Wire Contracts & E2E (cargo test --workspace)
  Gate 3: Offline Config Validation (open-ci validate)
  Gate 4: Real Compiled-Binary Runtime Proof (drive.sh + official @actions/cache client against open-ci cache) [skipped in --fast]
  Gate 5: TypeScript Routing Action (node --test)
  Gate 6: IaC, Packer & Security Policy Invariants (terraform fmt, bash -n, shellcheck, IMDSv2/zero-IAM)
  Gate 7: Route & Doc Parity (crates/open-ci-server/src/http/ <-> docs/architecture.md)
EOF
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $arg (use --fast or --help)" >&2
      exit 2
      ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

gate() {
  printf '\n==> %s\n' "$1"
}

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Gate 1: Static, Lint & Hexagonal Purity
# ---------------------------------------------------------------------------
gate "Gate 1/7: Static, Lint & Hexagonal Purity"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

if ALLOW_HITS="$(grep -rn '#\[allow(' crates/ || true)" && [[ -n "$ALLOW_HITS" ]]; then
  printf '%s\n' "$ALLOW_HITS" >&2
  fail "Found forbidden #[allow( attribute(s) in crates/. Fix the underlying lint violation instead of silencing Clippy."
fi
echo "  ok: zero #[allow( attributes in crates/"

if FSYNC_HITS="$(grep -rnE '\.sync_(all|data)\(' crates/open-ci-cache/src || true)" && [[ -n "$FSYNC_HITS" ]]; then
  printf '%s\n' "$FSYNC_HITS" >&2
  fail "open-ci-cache must not fsync: on ext4 data=ordered one fsync flushes every concurrent upload's dirty pages and stalls all saves. Publish with write + flush + atomic rename, and let a torn file read as a cache miss."
fi
echo "  ok: zero fsync calls in open-ci-cache (no ext4 journal-commit stampede)"

CORE_TREE="$(cargo tree -p open-ci-core -e normal)"
if FORBIDDEN_DEPS="$(printf '%s\n' "$CORE_TREE" | grep -E '(^|[[:space:]])(tokio|reqwest|rusqlite|axum|hyper)[[:space:]]' || true)" && [[ -n "$FORBIDDEN_DEPS" ]]; then
  printf '%s\n' "$FORBIDDEN_DEPS" >&2
  fail "Hexagonal purity violated: open-ci-core depends on forbidden I/O crate(s). Move side effects into open-ci-store, open-ci-server, open-ci-github, or open-ci-provider-*."
fi
echo "  ok: open-ci-core hexagonal purity verified (zero tokio/reqwest/rusqlite/axum/hyper)"

# ---------------------------------------------------------------------------
# Gate 2: Domain Invariants, Storage, Wire Contracts & E2E
# ---------------------------------------------------------------------------
gate "Gate 2/7: Domain Invariants, Storage, Wire Contracts & E2E"

cargo test --workspace

# ---------------------------------------------------------------------------
# Gate 3: Offline Config Validation
# ---------------------------------------------------------------------------
gate "Gate 3/7: Offline Config Validation"

cargo run --quiet --bin open-ci -- validate --config config/open-ci.example.toml
cargo run --quiet --bin open-ci -- validate --config config/open-ci.high-perf.example.toml
cargo run --quiet --bin open-ci -- validate --config config/open-ci.cloud-hypervisor.example.toml
cargo run --quiet --bin open-ci -- validate --config config/open-ci.hybrid-byoc.example.toml

# ---------------------------------------------------------------------------
# Gate 4: Real Compiled-Binary Runtime Proof (reusable driver)
# ---------------------------------------------------------------------------
gate "Gate 4/7: Real Compiled-Binary Runtime Proof (HTTP + SQLite DB Evidence)"

if [[ "$FAST_MODE" -eq 1 ]]; then
  echo "  SKIPPED (--fast mode)"
else
  DRIVE_SCRIPT=".agents/skills/verify-open-ci/scripts/drive.sh"
  if [[ ! -x "$DRIVE_SCRIPT" ]]; then
    fail "Missing executable runtime driver ${DRIVE_SCRIPT}. Restore it and run chmod +x ${DRIVE_SCRIPT}."
  fi
  "$DRIVE_SCRIPT"
  if command -v npm >/dev/null 2>&1; then
    ./scripts/cache-client-e2e.sh
  else
    echo "  warn: npm not installed; skipping official @actions/cache client proof (scripts/cache-client-e2e.sh)"
  fi
fi

# ---------------------------------------------------------------------------
# Gate 5: TypeScript Routing Action
# ---------------------------------------------------------------------------
gate "Gate 5/7: TypeScript Routing Action"

node --test actions/route/test/route.test.ts

# ---------------------------------------------------------------------------
# Gate 6: IaC, Packer & Security Policy Invariants
# ---------------------------------------------------------------------------
gate "Gate 6/7: IaC, Packer & Security Policy Invariants"

if command -v terraform >/dev/null 2>&1; then
  terraform fmt -check -recursive infra/terraform
  echo "  ok: terraform fmt -check passed"
else
  echo "  warn: terraform not installed; skipping terraform fmt -check"
fi

bash -n scripts/verify.sh scripts/cache-client-e2e.sh .agents/skills/verify-open-ci/scripts/drive.sh deploy/install.sh infra/packer/scripts/install-runner.sh infra/packer/scripts/setup-nvme.sh infra/cloud-hypervisor/setup-host.sh infra/cloud-hypervisor/build-rootfs.sh infra/scripts/connect-gcp-byoc.sh infra/scripts/setup-cache-nvme.sh
echo "  ok: bash -n passed for verify.sh, drive.sh, deploy/install.sh, Packer, Cloud Hypervisor, and BYOC scripts"

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck deploy/install.sh infra/packer/scripts/install-runner.sh infra/packer/scripts/setup-nvme.sh infra/cloud-hypervisor/setup-host.sh infra/cloud-hypervisor/build-rootfs.sh infra/scripts/connect-gcp-byoc.sh infra/scripts/setup-cache-nvme.sh
  echo "  ok: shellcheck passed for deploy/install.sh, Packer, Cloud Hypervisor, and BYOC scripts"
else
  echo "  warn: shellcheck not installed; skipping shellcheck"
fi

if ! grep -Eq 'http_tokens[[:space:]]*=[[:space:]]*"required"' infra/terraform/modules/aws-controller/main.tf; then
  fail "Policy check failed: set http_tokens = \"required\" in infra/terraform/modules/aws-controller/main.tf to enforce IMDSv2."
fi
echo "  ok: policy check AWS Terraform IMDSv2 http_tokens = \"required\""

if grep -Eq 'iam:PassRole' infra/terraform/modules/aws-byoc/main.tf infra/cloudformation/open-ci-aws-byoc.yaml; then
  fail "Policy check failed: AWS BYOC templates must never grant iam:PassRole (runners must stay zero-IAM)."
fi
if ! grep -Fq 'ec2:MetadataHttpTokens' infra/terraform/modules/aws-byoc/main.tf \
  || ! grep -Fq 'ec2:MetadataHttpTokens' infra/cloudformation/open-ci-aws-byoc.yaml; then
  fail "Policy check failed: AWS BYOC templates must enforce ec2:MetadataHttpTokens = required."
fi
if grep -Eq '"iam\.serviceAccounts\.actAs"|iam\.serviceAccounts\.actAs,' infra/terraform/modules/gcp-byoc/main.tf infra/scripts/connect-gcp-byoc.sh; then
  fail "Policy check failed: GCP BYOC templates must never grant iam.serviceAccounts.actAs (runners must stay zero-IAM)."
fi
for gcp_role_file in infra/terraform/modules/gcp-byoc/main.tf infra/terraform/modules/gcp-foundation/main.tf infra/scripts/connect-gcp-byoc.sh; do
  if ! grep -Eq '"compute\.subnetworks\.useExternalIp"|compute\.subnetworks\.useExternalIp,' "$gcp_role_file"; then
    fail "Policy check failed: grant compute.subnetworks.useExternalIp in ${gcp_role_file} so external_ipv4 = true runners egress directly (no Cloud NAT fees/caps) behind the zero-ingress runner tag."
  fi
done
echo "  ok: policy check BYOC IaC (zero iam:PassRole, zero actAs, IMDSv2 required, GCP useExternalIp granted)"

if ! grep -Fq '"MetadataOptions.HttpTokens"' crates/open-ci-provider-aws/src/params.rs \
  || ! grep -Eq '"MetadataOptions\.HttpTokens",[[:space:]]*"required"' crates/open-ci-provider-aws/src/params.rs; then
  fail "Policy check failed: pass (\"MetadataOptions.HttpTokens\", \"required\") in crates/open-ci-provider-aws/src/params.rs to enforce IMDSv2 on runners."
fi
echo "  ok: policy check AWS provider MetadataOptions.HttpTokens = required"

if ! grep -Fq '"serviceAccounts": []' crates/open-ci-provider-gcp/src/dto.rs; then
  fail "Policy check failed: keep \"serviceAccounts\": [] in crates/open-ci-provider-gcp/src/dto.rs so ephemeral GCP runners have zero IAM."
fi
echo "  ok: policy check GCP provider \"serviceAccounts\": [] (zero-IAM runners)"

if grep -Eq '^RuntimeDirectory=open-ci' deploy/systemd/open-ci.service; then
  fail "Policy check failed: remove RuntimeDirectory=open-ci from deploy/systemd/open-ci.service so open-ci-credentials.service (open-ci-broker) retains write ownership of /run/open-ci."
fi
if ! grep -Eq '^RuntimeDirectory=open-ci' deploy/systemd/open-ci-credentials.service \
  || ! grep -Eq '^RuntimeDirectoryMode=0750' deploy/systemd/open-ci-credentials.service; then
  fail "Policy check failed: deploy/systemd/open-ci-credentials.service must own RuntimeDirectory=open-ci with RuntimeDirectoryMode=0750."
fi
echo "  ok: policy check systemd /run/open-ci owned by open-ci-credentials.service (0750)"

# ---------------------------------------------------------------------------
# Gate 7: Route & Doc Parity
# ---------------------------------------------------------------------------
gate "Gate 7/7: Route & Doc Parity"

ARCH_DOC="docs/architecture.md"
if [[ ! -f "$ARCH_DOC" ]]; then
  fail "Missing architecture documentation file: ${ARCH_DOC}"
fi

EXTRACTED_ROUTES="$(
  awk '
    /\.route\(/ {
      line = $0
      while (line !~ /"\/[^"]*"/ && (getline next_line) > 0) {
        line = line " " next_line
      }
      if (match(line, /"\/[^"]*"/)) {
        print substr(line, RSTART + 1, RLENGTH - 2)
      }
    }
  ' crates/open-ci-server/src/http/*.rs | sort -u
)"

CANONICAL_ROUTES="$(cat <<'EOF'
/healthz
/readyz
/v1/ingress/webhook
/v1/ingress/admit
/v1/workers/{name}/bootstrap
/v1/workers/{name}/usage
/api/v1/overview
/api/v1/pools
/api/v1/workers
/api/v1/jobs
/api/v1/admissions
/api/v1/backoffs
/api/v1/audit
/api/v1/control/mode
/api/v1/reconcile
/api/v1/events
/metrics
EOF
)"

ALL_ROUTES="$(printf '%s\n%s\n' "$EXTRACTED_ROUTES" "$CANONICAL_ROUTES" | sed '/^$/d' | sort -u)"

MISSING_ROUTES=0
ROUTE_COUNT=0
while IFS= read -r route; do
  ROUTE_COUNT=$((ROUTE_COUNT + 1))
  if ! grep -Fq -- "$route" "$ARCH_DOC"; then
    printf '  MISSING in %s: %s\n' "$ARCH_DOC" "$route" >&2
    MISSING_ROUTES=$((MISSING_ROUTES + 1))
  fi
done <<< "$ALL_ROUTES"

if [[ "$MISSING_ROUTES" -gt 0 ]]; then
  fail "Route/doc parity failed: document the ${MISSING_ROUTES} missing route(s) in ${ARCH_DOC} and .agents/skills/verify-open-ci/features.md."
fi
echo "  ok: all ${ROUTE_COUNT} registered HTTP routes are documented in ${ARCH_DOC}"

printf '\nALL 7 GATES PASSED%s.\n' "$( [[ "$FAST_MODE" -eq 1 ]] && echo " (--fast mode)" || echo "" )"
