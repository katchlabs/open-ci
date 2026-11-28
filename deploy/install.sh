#!/usr/bin/env bash
# Single-command idempotent Linux controller installer for open-ci.
# Downloads and SHA-256 verifies open-ci, creates hardened system users and 0600 secrets,
# generates a validated /etc/open-ci/open-ci.toml via `open-ci init`, and installs systemd units.
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "ERROR: deploy/install.sh must be run as root (try: sudo $0 ...)" >&2
  exit 1
fi

PRESET="gcp-spot"
REPO=""
ORG=""
DOMAIN="ci.example.com"
GCP_PROJECT="your-gcp-project-id"
AWS_ACCOUNT="123456789012"
VERSION="latest"
LOCAL_BINARY=""
WITH_CACHE="false"
ARM64="false"
FORCE="false"

usage() {
  cat <<'EOF'
Usage: sudo deploy/install.sh [options]

Options:
  --preset <name>       gcp-spot | aws-spot | multi-cloud | bare-metal | hybrid (default: gcp-spot)
  --repo <owner/repo>   Single GitHub repository scope
  --org <owner>         Organization-wide GitHub scope
  --domain <host>       Public HTTPS domain (default: ci.example.com)
  --gcp-project <id>    GCP project ID
  --aws-account <id>    12-digit AWS account ID
  --with-cache          Enable built-in transparent Actions v2 cache (`open-ci serve --with-cache`)
  --arm64               Include an ARM64 runner pool alongside x64
  --version <tag>       Release tag (e.g. v0.2.0) or "latest" (default: latest)
  --binary <path>       Install a pre-downloaded local open-ci binary instead of downloading
  --force               Overwrite existing /etc/open-ci/open-ci.toml
  -h, --help            Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --preset) PRESET="$2"; shift 2 ;;
    --repo) REPO="$2"; shift 2 ;;
    --org) ORG="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --gcp-project) GCP_PROJECT="$2"; shift 2 ;;
    --aws-account) AWS_ACCOUNT="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --binary) LOCAL_BINARY="$2"; shift 2 ;;
    --with-cache) WITH_CACHE="true"; shift ;;
    --arm64) ARM64="true"; shift ;;
    --force) FORCE="true"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "ERROR: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "${PRESET}" in
  gcp-spot|aws-spot|multi-cloud|bare-metal|hybrid) ;;
  *) echo "ERROR: invalid --preset '${PRESET}'" >&2; exit 2 ;;
esac

if [[ -n "${REPO}" && -n "${ORG}" ]]; then
  echo "ERROR: pass either --repo or --org, not both" >&2
  exit 2
fi

HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
  x86_64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "ERROR: unsupported Linux architecture '${HOST_ARCH}'" >&2; exit 1 ;;
esac

TMP_DIR="$(mktemp -d /tmp/open-ci-install.XXXXXX)"
cleanup() {
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

install_binary() {
  if [[ -n "${LOCAL_BINARY}" ]]; then
    if [[ ! -f "${LOCAL_BINARY}" ]]; then
      echo "ERROR: --binary path does not exist: ${LOCAL_BINARY}" >&2
      exit 1
    fi
    install -m 0755 "${LOCAL_BINARY}" /usr/local/bin/open-ci
    return 0
  fi

  local base_url
  if [[ "${VERSION}" == "latest" ]]; then
    base_url="https://github.com/katchlabs/open-ci/releases/latest/download"
  else
    base_url="https://github.com/katchlabs/open-ci/releases/download/${VERSION}"
  fi
  local archive="open-ci-${TARGET_TRIPLE}.tar.gz"
  echo "open-ci-install: downloading ${archive} (${VERSION})"
  curl -fsSL --retry 3 "${base_url}/${archive}" -o "${TMP_DIR}/${archive}"
  curl -fsSL --retry 3 "${base_url}/SHA256SUMS.txt" -o "${TMP_DIR}/SHA256SUMS.txt"
  (cd "${TMP_DIR}" && sha256sum --ignore-missing -c SHA256SUMS.txt)
  tar -xzf "${TMP_DIR}/${archive}" -C "${TMP_DIR}"
  install -m 0755 "${TMP_DIR}/open-ci" /usr/local/bin/open-ci
}

create_users_and_dirs() {
  if ! id -u open-ci >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/open-ci --shell /usr/sbin/nologin open-ci
  fi
  if ! id -u open-ci-broker >/dev/null 2>&1; then
    useradd --system --no-create-home --gid open-ci --shell /usr/sbin/nologin open-ci-broker
  fi
  install -d -o root -g open-ci -m 0750 /etc/open-ci
  install -d -o open-ci -g open-ci -m 0700 /var/lib/open-ci /var/lib/open-ci-cache /var/backups/open-ci
  install -d -o open-ci-broker -g open-ci -m 0750 /run/open-ci
}

ensure_secret() {
  local path="$1"
  local hex_bytes="$2"
  if [[ ! -f "${path}" ]]; then
    openssl rand -hex "${hex_bytes}" > "${path}"
    chown open-ci:open-ci "${path}"
    chmod 0600 "${path}"
  fi
}

create_secrets_and_env() {
  ensure_secret /etc/open-ci/webhook-secret 32
  ensure_secret /etc/open-ci/bootstrap-secret 32
  ensure_secret /etc/open-ci/operator-token 24
  if [[ "${WITH_CACHE}" == "true" ]]; then
    ensure_secret /etc/open-ci/cache-signing-key 32
  fi

  cat <<EOF > /etc/open-ci/open-ci.env
OPEN_CI_GITHUB_TOKEN_FILE=/run/open-ci/github-token
OPEN_CI_WEBHOOK_SECRET_FILE=/etc/open-ci/webhook-secret
OPEN_CI_BOOTSTRAP_SECRET_FILE=/etc/open-ci/bootstrap-secret
OPEN_CI_OPERATOR_TOKEN_FILE=/etc/open-ci/operator-token
EOF
  if [[ "${WITH_CACHE}" == "true" ]]; then
    printf 'OPEN_CI_CACHE_SIGNING_KEY_FILE=/etc/open-ci/cache-signing-key\n' >> /etc/open-ci/open-ci.env
  fi
  chown open-ci:open-ci /etc/open-ci/open-ci.env
  chmod 0600 /etc/open-ci/open-ci.env
}

generate_config() {
  if [[ -f /etc/open-ci/open-ci.toml && "${FORCE}" != "true" ]]; then
    echo "open-ci-install: keeping existing /etc/open-ci/open-ci.toml (pass --force to overwrite)"
    return 0
  fi

  local init_args=(
    init
    --preset "${PRESET}"
    --public-url "https://${DOMAIN}"
    --gcp-project "${GCP_PROJECT}"
    --aws-account "${AWS_ACCOUNT}"
    --output /etc/open-ci/open-ci.toml
    --force
  )
  if [[ -n "${REPO}" ]]; then
    init_args+=(--repo "${REPO}")
  elif [[ -n "${ORG}" ]]; then
    init_args+=(--org "${ORG}")
  fi
  if [[ "${WITH_CACHE}" == "true" ]]; then
    init_args+=(--with-cache)
  fi
  if [[ "${ARM64}" == "true" ]]; then
    init_args+=(--arm64)
  fi

  /usr/local/bin/open-ci "${init_args[@]}"
  chown root:open-ci /etc/open-ci/open-ci.toml
  chmod 0640 /etc/open-ci/open-ci.toml
}

install_systemd_units() {
  local exec_flags=""
  if [[ "${WITH_CACHE}" == "true" ]]; then
    exec_flags=" --with-cache"
  fi

  cat <<EOF > /etc/systemd/system/open-ci.service
[Unit]
Description=open-ci Control Plane
After=network-online.target open-ci-credentials.service
Wants=network-online.target open-ci-credentials.service

[Service]
Type=simple
User=open-ci
Group=open-ci
EnvironmentFile=/etc/open-ci/open-ci.env
ExecStartPre=/usr/local/bin/open-ci validate --config /etc/open-ci/open-ci.toml
ExecStart=/usr/local/bin/open-ci serve --config /etc/open-ci/open-ci.toml${exec_flags}
Restart=on-failure
RestartSec=3
TimeoutStopSec=30
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictRealtime=true
RestrictNamespaces=true
LockPersonality=true
ReadWritePaths=/var/lib/open-ci /var/lib/open-ci-cache
ReadOnlyPaths=/etc/open-ci /run/open-ci
StateDirectory=open-ci
UMask=0077

[Install]
WantedBy=multi-user.target
EOF

  if [[ "${PRESET}" == "bare-metal" || "${PRESET}" == "hybrid" ]]; then
    mkdir -p /etc/systemd/system/open-ci.service.d
    cat <<'EOF' > /etc/systemd/system/open-ci.service.d/cloud-hypervisor.conf
[Service]
PrivateDevices=no
DeviceAllow=/dev/kvm rw
DeviceAllow=/dev/net/tun rw
AmbientCapabilities=CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_ADMIN
EOF
  fi

  if [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
  fi
}

install_binary
create_users_and_dirs
create_secrets_and_env
generate_config
install_systemd_units

CACHE_FLAG=""
if [[ "${WITH_CACHE}" == "true" ]]; then
  CACHE_FLAG=" --with-cache"
fi

echo ""
echo "open-ci installed successfully!"
echo "Remaining steps:"
echo "  1. Install your GitHub App private key at /etc/open-ci/github-app.pem (0600, owned by open-ci-broker) and configure /etc/open-ci/broker.toml"
echo "  2. Fill in cloud subnet/image placeholders in /etc/open-ci/open-ci.toml"
echo "  3. Verify readiness: sudo -u open-ci bash -c 'set -a; source /etc/open-ci/open-ci.env; open-ci doctor --config /etc/open-ci/open-ci.toml${CACHE_FLAG}'"
echo "  4. Start:            sudo systemctl enable --now open-ci.service"
if [[ "${WITH_CACHE}" == "true" ]]; then
  echo "  Cache on local NVMe: format it journal-less (mkfs.ext4 -F -O ^has_journal -m 0) and mount -o noatime (no discard)"
  echo "                      at /var/lib/open-ci-cache before step 4 with infra/scripts/setup-cache-nvme.sh --device <disk> --install-unit (docs/cache.md)"
fi
