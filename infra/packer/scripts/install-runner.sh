#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_ARCH:?RUNNER_ARCH is required (x64 or arm64)}"
: "${RUNNER_VERSION:?RUNNER_VERSION is required}"
: "${RUNNER_SHA256:?RUNNER_SHA256 is required}"

INSTALL_DOCKER="${INSTALL_DOCKER:-true}"

export DEBIAN_FRONTEND=noninteractive

cat <<'EOF' > /etc/dpkg/dpkg.cfg.d/01-open-ci-speedup
force-unsafe-io
EOF

cat <<'EOF' > /etc/apt/apt.conf.d/99-open-ci-speedup
Acquire::Languages "none";
Acquire::GzipIndexes "false";
Acquire::http::Pipeline-Depth "10";
Acquire::Queue-Mode "host";
EOF

if command -v debconf-set-selections >/dev/null 2>&1; then
  printf 'man-db man-db/auto-update boolean false\n' | debconf-set-selections
fi
rm -f /var/lib/man-db/auto-update
if dpkg -s needrestart >/dev/null 2>&1; then
  apt-get purge -y -qq needrestart || true
fi

apt-get update -qq
PACKAGES=(
  build-essential
  ca-certificates
  curl
  git
  jq
  mdadm
  nvme-cli
  pigz
  python3
  tar
  unzip
  xfsprogs
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
Before=docker.service cloud-final.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/open-ci-setup-nvme.sh

[Install]
WantedBy=multi-user.target
EOF
systemctl enable open-ci-nvme.service

cat <<'EOF' > /etc/sysctl.d/99-open-ci.conf
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
vm.swappiness = 1
vm.dirty_ratio = 40
vm.dirty_background_ratio = 10
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = cubic
net.core.rmem_max = 67108864
net.core.wmem_max = 67108864
net.core.rmem_default = 262144
net.core.wmem_default = 262144
net.ipv4.tcp_rmem = 4096 262144 67108864
net.ipv4.tcp_wmem = 4096 262144 67108864
net.ipv4.tcp_adv_win_scale = 1
net.ipv4.tcp_no_metrics_save = 1
net.ipv4.tcp_fastopen = 3
net.core.netdev_max_backlog = 32768
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_mtu_probing = 1
EOF

systemctl mask \
  apt-daily.timer \
  apt-daily-upgrade.timer \
  apt-daily.service \
  apt-daily-upgrade.service \
  unattended-upgrades.service \
  man-db.timer 2>/dev/null || true

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
echo "runner ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/open-ci-runner
chmod 0440 /etc/sudoers.d/open-ci-runner
visudo -cf /etc/sudoers.d/open-ci-runner >/dev/null

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
rm -f /var/lib/man-db/auto-update
apt-get clean
rm -rf /tmp/* /var/tmp/*
rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
cloud-init clean --logs || true
