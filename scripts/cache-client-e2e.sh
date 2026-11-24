#!/usr/bin/env bash
# Proves the compiled open-ci cache server against the official @actions/cache toolkit client.
# Usage: ./scripts/cache-client-e2e.sh [@actions/cache version]
set -euo pipefail

TOOLKIT_VERSION="${1:-6.3.0}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/open-ci-cache-e2e.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

cd "$REPO_ROOT"
cargo build --quiet --bin open-ci

npm install --silent --no-audit --no-fund --prefix "$WORK/toolkit" "@actions/cache@${TOOLKIT_VERSION}" >/dev/null
echo "  using @actions/cache@${TOOLKIT_VERSION}"

node scripts/cache-client-e2e.mjs \
  "$REPO_ROOT/target/debug/open-ci" \
  "$WORK/toolkit/node_modules/@actions/cache" \
  "$WORK/run"
