#!/usr/bin/env bash
# Idempotent open-ci runner host tuning shared by the Packer bake (install-runner.sh) and the
# stock Ubuntu 24.04 fallback: dpkg force-unsafe-io, apt speedups, /etc/sysctl.d/99-open-ci.conf,
# masked apt timers, and open-ci-nvme.service (page-cache ext4 on local NVMe at /mnt/nvme).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: sudo tune-host.sh [--now]

Installs open-ci runner host tuning on Ubuntu 24.04. Safe to re-run.

  --now       Also apply sysctls and mount local NVMe immediately (stock VMs booted before baking)
  -h, --help  Show this help message

Environment:
  OPEN_CI_SETUP_NVME_SRC  Path to setup-nvme.sh (default: next to this script)
EOF
}

APPLY_NOW=0
for arg in "$@"; do
  case "${arg}" in
    --now) APPLY_NOW=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'ERROR: unknown option: %s\n' "${arg}" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ "${EUID}" -ne 0 ]]; then
  echo "ERROR: tune-host.sh must run as root (sudo)." >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SETUP_NVME_SRC="${OPEN_CI_SETUP_NVME_SRC:-${SCRIPT_DIR}/setup-nvme.sh}"
SETUP_NVME_DST="/usr/local/sbin/open-ci-setup-nvme.sh"
NVME_UNIT="/etc/systemd/system/open-ci-nvme.service"
SYSCTL_CONF="/etc/sysctl.d/99-open-ci.conf"

export DEBIAN_FRONTEND=noninteractive

mkdir -p /etc/dpkg/dpkg.cfg.d /etc/apt/apt.conf.d /etc/sysctl.d /usr/local/sbin

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

MISSING_PACKAGES=()
command -v mkfs.xfs >/dev/null 2>&1 || MISSING_PACKAGES+=(xfsprogs)
command -v mdadm >/dev/null 2>&1 || MISSING_PACKAGES+=(mdadm)
command -v losetup >/dev/null 2>&1 || MISSING_PACKAGES+=(util-linux)
command -v jq >/dev/null 2>&1 || MISSING_PACKAGES+=(jq)
if [[ "${#MISSING_PACKAGES[@]}" -gt 0 ]]; then
  echo "open-ci-tune: installing local NVMe prerequisites: ${MISSING_PACKAGES[*]}"
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends "${MISSING_PACKAGES[@]}"
fi

if [[ -f "${SETUP_NVME_SRC}" ]]; then
  if [[ "$(readlink -f "${SETUP_NVME_SRC}")" != "$(readlink -f "${SETUP_NVME_DST}" 2>/dev/null || true)" ]]; then
    install -m 0755 "${SETUP_NVME_SRC}" "${SETUP_NVME_DST}"
  fi
elif [[ ! -x "${SETUP_NVME_DST}" ]]; then
  echo "ERROR: setup-nvme.sh not found at ${SETUP_NVME_SRC} (set OPEN_CI_SETUP_NVME_SRC)." >&2
  exit 1
fi

cat <<EOF > "${NVME_UNIT}"
[Unit]
Description=Auto-configure local ephemeral NVMe storage for open-ci runner
After=local-fs.target
Before=docker.service cloud-final.service google-startup-scripts.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=${SETUP_NVME_DST}

[Install]
WantedBy=multi-user.target
EOF

cat <<'EOF' > "${SYSCTL_CONF}"
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

systemctl daemon-reload
systemctl enable open-ci-nvme.service >/dev/null 2>&1

systemctl mask \
  apt-daily.timer \
  apt-daily-upgrade.timer \
  apt-daily.service \
  apt-daily-upgrade.service \
  unattended-upgrades.service \
  man-db.timer >/dev/null 2>&1 || true

if [[ "${APPLY_NOW}" -eq 1 ]]; then
  systemctl stop apt-daily.service apt-daily-upgrade.service unattended-upgrades.service >/dev/null 2>&1 || true
  sysctl -q -p "${SYSCTL_CONF}" || true
  systemctl restart open-ci-nvme.service
  if mountpoint -q /mnt/nvme; then
    echo "open-ci-tune: /mnt/nvme mounted from $(findmnt -n -o SOURCE /mnt/nvme)"
  else
    echo "open-ci-tune: no local NVMe detected; runner work stays on the root volume"
  fi
fi

echo "open-ci-tune: host tuning installed (${SYSCTL_CONF}, dpkg force-unsafe-io, open-ci-nvme.service)"
