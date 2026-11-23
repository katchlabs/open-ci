#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_ARCH:?RUNNER_ARCH is required (x64 or arm64)}"
: "${RUNNER_VERSION:?RUNNER_VERSION is required}"
: "${RUNNER_SHA256:?RUNNER_SHA256 is required}"

INSTALL_DOCKER="${INSTALL_DOCKER:-true}"

export DEBIAN_FRONTEND=noninteractive

apt-get update -qq
PACKAGES=(
  ca-certificates
  curl
  git
  jq
  mdadm
  nvme-cli
  pigz
  tar
  unzip
  zstd
)
if [[ "${INSTALL_DOCKER}" == "true" ]]; then
  PACKAGES+=(docker.io docker-compose-v2)
  if apt-cache show docker-buildx-plugin >/dev/null 2>&1; then
    PACKAGES+=(docker-buildx-plugin)
  elif apt-cache show docker-buildx >/dev/null 2>&1; then
    PACKAGES+=(docker-buildx)
  fi
fi
apt-get install -y --no-install-recommends "${PACKAGES[@]}"

# Install the open-ci agent binary baked into the image.
install -m 0755 /tmp/open-ci /usr/local/bin/open-ci
rm -f /tmp/open-ci

# Install boot-time local NVMe ephemeral SSD setup script and systemd oneshot unit.
if [[ -f /tmp/setup-nvme.sh ]]; then
  install -m 0755 /tmp/setup-nvme.sh /usr/local/sbin/open-ci-setup-nvme.sh
  rm -f /tmp/setup-nvme.sh
fi

cat <<'EOF' > /etc/systemd/system/open-ci-nvme.service
[Unit]
Description=Auto-configure local ephemeral NVMe storage for open-ci runner
After=local-fs.target
Before=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/open-ci-setup-nvme.sh

[Install]
WantedBy=multi-user.target
EOF
systemctl enable open-ci-nvme.service

# Tune kernel parameters for high-concurrency builds and large JS/Rust monorepos.
cat <<'EOF' > /etc/sysctl.d/99-open-ci.conf
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
vm.swappiness = 10
EOF

# Configure Docker Engine for high-speed CI (BuildKit, overlay2, parallel pulls/pushes).
if [[ "${INSTALL_DOCKER}" == "true" ]]; then
  mkdir -p /etc/docker
  cat <<'EOF' > /etc/docker/daemon.json
{
  "storage-driver": "overlay2",
  "features": {
    "buildkit": true
  },
  "max-concurrent-downloads": 10,
  "max-concurrent-uploads": 10,
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "50m",
    "max-file": "3"
  }
}
EOF
fi

# Create dedicated non-root runner user (with Docker group only when Docker is enabled).
if ! id -u runner >/dev/null 2>&1; then
  if [[ "${INSTALL_DOCKER}" == "true" ]]; then
    useradd --create-home --shell /bin/bash --groups docker runner
  else
    useradd --create-home --shell /bin/bash runner
  fi
fi

# Download and verify the pinned GitHub Actions runner release.
RUNNER_DIR="/opt/actions-runner"
ARCHIVE="actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz"
DOWNLOAD_URL="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/${ARCHIVE}"

mkdir -p "${RUNNER_DIR}"
curl -fsSL "${DOWNLOAD_URL}" -o "/tmp/${ARCHIVE}"
echo "${RUNNER_SHA256}  /tmp/${ARCHIVE}" | sha256sum -c -
tar -xzf "/tmp/${ARCHIVE}" -C "${RUNNER_DIR}"
rm -f "/tmp/${ARCHIVE}"

"${RUNNER_DIR}/bin/installdependencies.sh"
chown -R runner:runner "${RUNNER_DIR}"

# Scrub machine-unique identifiers and cloud-init state so every ephemeral VM boots clean.
if [[ "${INSTALL_DOCKER}" == "true" ]]; then
  systemctl enable docker
fi
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
cloud-init clean --logs || true
