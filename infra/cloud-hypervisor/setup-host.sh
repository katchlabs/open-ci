#!/usr/bin/env bash
# Production Linux host setup for running open-ci Cloud Hypervisor microVMs on
# bare-metal or nested-KVM Ubuntu 24.04 hosts (GCP c4d/c3, AWS c8a.metal, OVH/Hetzner).
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "ERROR: setup-host.sh must be run as root (try: sudo $0)" >&2
  exit 1
fi

CH_VERSION="${CH_VERSION:-v44.0}"
CH_BIN="${CH_BIN:-/usr/local/bin/cloud-hypervisor}"
CH_SHA256="${CH_SHA256:-}"
CH_BINARY_PATH="${CH_BINARY_PATH:-}"
FORCE_CH_INSTALL="${FORCE_CH_INSTALL:-false}"

DATA_DIR="${DATA_DIR:-/var/lib/open-ci}"
BRIDGE_NAME="${BRIDGE_NAME:-open-ci-br0}"
BRIDGE_CIDR="${BRIDGE_CIDR:-10.200.0.1/16}"
SUBNET_CIDR="${SUBNET_CIDR:-10.200.0.0/16}"
CACHE_PORT="${CACHE_PORT:-8090}"
EGRESS_IFACE="${EGRESS_IFACE:-}"
HUGEPAGES_GB="${HUGEPAGES_GB:-0}"
DISABLE_CSTATES="${OPEN_CI_DISABLE_CSTATES:-false}"

if [[ ! "${HUGEPAGES_GB}" =~ ^[0-9]+$ ]]; then
  echo "ERROR: HUGEPAGES_GB must be a non-negative integer (got '${HUGEPAGES_GB}')" >&2
  exit 1
fi
if [[ ! "${CACHE_PORT}" =~ ^[0-9]+$ ]] || (( CACHE_PORT < 1 || CACHE_PORT > 65535 )); then
  echo "ERROR: CACHE_PORT must be a valid TCP port (got '${CACHE_PORT}')" >&2
  exit 1
fi

HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
  x86_64)
    CH_ASSET="cloud-hypervisor-static"
    ;;
  aarch64|arm64)
    CH_ASSET="cloud-hypervisor-static-aarch64"
    ;;
  *)
    echo "ERROR: unsupported host architecture '${HOST_ARCH}' (expected x86_64 or aarch64)" >&2
    exit 1
    ;;
esac

log() {
  printf 'open-ci-ch-host: %s\n' "$*"
}

tune_block_device() {
  local dev="$1"
  local name
  local qdir
  name="$(basename "${dev}")"
  qdir="/sys/block/${name}/queue"
  [[ -d "${qdir}" ]] || return 0

  if [[ -w "${qdir}/scheduler" ]] && grep -qw "none" "${qdir}/scheduler" 2>/dev/null; then
    printf 'none' > "${qdir}/scheduler" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/read_ahead_kb" ]]; then
    printf '4096' > "${qdir}/read_ahead_kb" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/nr_requests" ]]; then
    printf '1024' > "${qdir}/nr_requests" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/add_random" ]]; then
    printf '0' > "${qdir}/add_random" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/iostats" ]]; then
    printf '0' > "${qdir}/iostats" 2>/dev/null || true
  fi
}

install_host_packages() {
  export DEBIAN_FRONTEND=noninteractive
  log "installing required host packages"
  apt-get update -qq
  apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    debootstrap \
    e2fsprogs \
    iproute2 \
    iptables \
    jq \
    mdadm \
    pigz \
    xfsprogs \
    zstd
}

TMP_CH_BIN=""
cleanup_tmp() {
  if [[ -n "${TMP_CH_BIN}" ]]; then
    rm -f "${TMP_CH_BIN}"
  fi
}
trap cleanup_tmp EXIT

install_cloud_hypervisor() {
  if [[ "${FORCE_CH_INSTALL}" != "true" ]] && [[ -x "${CH_BIN}" ]]; then
    if "${CH_BIN}" --version 2>/dev/null | grep -Fq "${CH_VERSION}"; then
      log "cloud-hypervisor ${CH_VERSION} already installed at ${CH_BIN}"
      return 0
    fi
  fi

  TMP_CH_BIN="$(mktemp /tmp/cloud-hypervisor.XXXXXX)"

  if [[ -n "${CH_BINARY_PATH}" ]]; then
    if [[ ! -f "${CH_BINARY_PATH}" ]]; then
      echo "ERROR: CH_BINARY_PATH does not exist: ${CH_BINARY_PATH}" >&2
      exit 1
    fi
    cp "${CH_BINARY_PATH}" "${TMP_CH_BIN}"
  else
    local url="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/${CH_ASSET}"
    log "downloading ${url}"
    curl -fsSL --retry 3 --retry-delay 2 "${url}" -o "${TMP_CH_BIN}"
  fi

  if [[ -n "${CH_SHA256}" ]]; then
    printf '%s  %s\n' "${CH_SHA256}" "${TMP_CH_BIN}" | sha256sum -c -
  fi

  install -m 0755 "${TMP_CH_BIN}" "${CH_BIN}"
  rm -f "${TMP_CH_BIN}"
  TMP_CH_BIN=""
  log "installed $("${CH_BIN}" --version)"
}

verify_kvm() {
  if [[ ! -e /dev/kvm ]]; then
    modprobe kvm 2>/dev/null || true
    if [[ "${HOST_ARCH}" == "x86_64" ]]; then
      modprobe kvm_intel 2>/dev/null || modprobe kvm_amd 2>/dev/null || true
    fi
  fi

  if [[ ! -e /dev/kvm ]]; then
    echo "ERROR: /dev/kvm is missing. Enable nested virtualization (e.g. GCP --enable-nested-virtualization) or run on a bare-metal host." >&2
    exit 1
  fi
  if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
    echo "ERROR: /dev/kvm exists but is not readable/writable by root (check permissions/LSM policy)." >&2
    exit 1
  fi
  log "/dev/kvm verified"
}

ensure_data_directories() {
  mkdir -p "${DATA_DIR}/microvms" "${DATA_DIR}/images" "${DATA_DIR}/cache/blobs"
  chmod 0755 "${DATA_DIR}" "${DATA_DIR}/images"
  chmod 0750 "${DATA_DIR}/microvms" "${DATA_DIR}/cache" "${DATA_DIR}/cache/blobs"
  if id -u open-ci >/dev/null 2>&1; then
    chown -R open-ci:open-ci "${DATA_DIR}/microvms" "${DATA_DIR}/cache"
  fi
}

persist_fstab_entry() {
  local target_dev="$1"
  local uuid
  uuid="$(blkid -s UUID -o value "${target_dev}" 2>/dev/null || true)"
  [[ -n "${uuid}" ]] || return 0
  if ! grep -Fq "UUID=${uuid}" /etc/fstab 2>/dev/null; then
    printf 'UUID=%s %s xfs noatime,nodiratime,logbufs=8,logbsize=256k,nofail,x-systemd.device-timeout=10 0 0\n' \
      "${uuid}" "${DATA_DIR}" >> /etc/fstab
  fi
}

setup_nvme_xfs() {
  if mountpoint -q "${DATA_DIR}"; then
    local existing_src existing_fstype
    existing_src="$(findmnt -n -o SOURCE "${DATA_DIR}" 2>/dev/null || true)"
    existing_fstype="$(findmnt -n -o FSTYPE "${DATA_DIR}" 2>/dev/null || true)"
    if [[ -n "${existing_src}" ]]; then
      tune_block_device "${existing_src}"
    fi
    if [[ "${existing_fstype}" != "xfs" ]]; then
      log "WARNING: ${DATA_DIR} is mounted as '${existing_fstype}' (XFS with reflink=1 recommended for sub-2ms CoW rootfs clones)"
    else
      log "${DATA_DIR} already mounted on ${existing_src} (xfs)"
    fi
    ensure_data_directories
    return 0
  fi

  local root_src root_disk="" root_real parent_name
  root_src="$(findmnt -n -o SOURCE / 2>/dev/null || true)"
  if [[ -n "${root_src}" ]]; then
    root_real="$(readlink -f "${root_src}" 2>/dev/null || echo "${root_src}")"
    parent_name="$(lsblk -no PKNAME "${root_real}" 2>/dev/null | head -n1 || true)"
    if [[ -n "${parent_name}" ]]; then
      root_disk="/dev/${parent_name}"
    else
      root_disk="${root_real}"
    fi
  fi

  local nvme_devs=()
  local dev base_name model
  while IFS= read -r dev; do
    [[ -n "${dev}" ]] || continue
    [[ "${dev}" =~ ^/dev/nvme[0-9]+n[0-9]+$ ]] || continue
    [[ "${dev}" != "${root_disk}" ]] || continue

    if [[ "$(lsblk -nr "${dev}" 2>/dev/null | wc -l)" -ne 1 ]]; then
      continue
    fi
    if [[ -n "$(lsblk -dnro MOUNTPOINT "${dev}" 2>/dev/null)" ]]; then
      continue
    fi

    base_name="$(basename "${dev}")"
    if [[ -d "/sys/block/${base_name}/holders" ]] && [[ -n "$(ls -A "/sys/block/${base_name}/holders" 2>/dev/null)" ]]; then
      continue
    fi

    model="$(lsblk -dnro MODEL "${dev}" 2>/dev/null || true)"
    if [[ "${model}" == *"Elastic Block Store"* \
       || "${model}" == *"PersistentDisk"* \
       || "${model}" == *"Persistent Disk"* \
       || "${model}" == *"Hyperdisk"* \
       || "${model}" == *"nvme_card-pd"* ]]; then
      continue
    fi

    nvme_devs+=("${dev}")
  done < <(lsblk -dnpo NAME,TYPE 2>/dev/null | awk '$2 == "disk" { print $1 }')

  if [[ "${#nvme_devs[@]}" -eq 0 ]]; then
    log "no unmounted local ephemeral NVMe drives found; using existing filesystem for ${DATA_DIR}"
    ensure_data_directories
    return 0
  fi

  local target_dev
  if [[ "${#nvme_devs[@]}" -eq 1 ]]; then
    target_dev="${nvme_devs[0]}"
    log "formatting single local NVMe device ${target_dev} as XFS (reflink=1,crc=1)"
    mkfs.xfs -f -m reflink=1,crc=1 "${target_dev}"
  else
    target_dev="/dev/md0"
    if [[ -b "${target_dev}" ]]; then
      echo "ERROR: ${target_dev} already exists; mount it at ${DATA_DIR} or remove it before re-running setup-host.sh" >&2
      exit 1
    fi
    log "creating RAID0 ${target_dev} across ${#nvme_devs[@]} NVMe devices: ${nvme_devs[*]}"
    mdadm --create --verbose "${target_dev}" \
      --level=0 \
      --raid-devices="${#nvme_devs[@]}" \
      --chunk=256 \
      --force \
      "${nvme_devs[@]}"
    udevadm settle || true
    mkdir -p /etc/mdadm
    if ! grep -Fq "${target_dev}" /etc/mdadm/mdadm.conf 2>/dev/null; then
      mdadm --detail --scan >> /etc/mdadm/mdadm.conf
    fi
    log "formatting ${target_dev} as XFS (reflink=1,crc=1)"
    mkfs.xfs -f -m reflink=1,crc=1 "${target_dev}"
  fi

  for dev in "${nvme_devs[@]}"; do
    tune_block_device "${dev}"
  done
  if [[ "${target_dev}" != "${nvme_devs[0]}" ]]; then
    tune_block_device "${target_dev}"
  fi

  mkdir -p "${DATA_DIR}"
  mount -t xfs -o noatime,nodiratime,logbufs=8,logbsize=256k "${target_dev}" "${DATA_DIR}"
  persist_fstab_entry "${target_dev}"
  ensure_data_directories
  log "mounted ${target_dev} at ${DATA_DIR} (xfs reflink=1)"
}

install_bridge_service() {
  cat <<EOF > /usr/local/sbin/open-ci-setup-bridge.sh
#!/usr/bin/env bash
# Idempotent bridge, NAT masquerade, inter-VM isolation, and Zero-IAM metadata firewall for open-ci microVMs.
set -euo pipefail

BRIDGE_NAME="\${BRIDGE_NAME:-${BRIDGE_NAME}}"
BRIDGE_CIDR="\${BRIDGE_CIDR:-${BRIDGE_CIDR}}"
SUBNET_CIDR="\${SUBNET_CIDR:-${SUBNET_CIDR}}"
CACHE_PORT="\${CACHE_PORT:-${CACHE_PORT}}"
EGRESS_IFACE="\${EGRESS_IFACE:-${EGRESS_IFACE}}"

if [[ -z "\${EGRESS_IFACE}" ]]; then
  EGRESS_IFACE="\$(ip -4 route show default | awk '{for(i=1;i<=NF;i++) if(\$i=="dev"){print \$(i+1); exit}}')"
fi
if [[ -z "\${EGRESS_IFACE}" ]]; then
  echo "ERROR: failed to detect default IPv4 egress interface; set EGRESS_IFACE" >&2
  exit 1
fi

if ! ip link show "\${BRIDGE_NAME}" >/dev/null 2>&1; then
  ip link add name "\${BRIDGE_NAME}" type bridge
fi
sysctl -q -w "net.ipv6.conf.\${BRIDGE_NAME}.disable_ipv6=1" 2>/dev/null || true
ip addr replace "\${BRIDGE_CIDR}" dev "\${BRIDGE_NAME}"
ip link set "\${BRIDGE_NAME}" up

sysctl -q -w net.ipv4.ip_forward=1

ensure_top_rule() {
  local chain="\$1"
  shift
  while iptables -w 5 -C "\${chain}" "\$@" 2>/dev/null; do
    iptables -w 5 -D "\${chain}" "\$@"
  done
  iptables -w 5 -I "\${chain}" 1 "\$@"
}

# Outbound NATed internet traffic from microVMs.
if ! iptables -w 5 -C FORWARD -i "\${EGRESS_IFACE}" -o "\${BRIDGE_NAME}" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null; then
  iptables -w 5 -I FORWARD 1 -i "\${EGRESS_IFACE}" -o "\${BRIDGE_NAME}" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
fi
if ! iptables -w 5 -C FORWARD -i "\${BRIDGE_NAME}" -o "\${EGRESS_IFACE}" -j ACCEPT 2>/dev/null; then
  iptables -w 5 -I FORWARD 1 -i "\${BRIDGE_NAME}" -o "\${EGRESS_IFACE}" -j ACCEPT
fi
if ! iptables -w 5 -t nat -C POSTROUTING -s "\${SUBNET_CIDR}" -o "\${EGRESS_IFACE}" -j MASQUERADE 2>/dev/null; then
  iptables -w 5 -t nat -A POSTROUTING -s "\${SUBNET_CIDR}" -o "\${EGRESS_IFACE}" -j MASQUERADE
fi

# Block microVM-to-microVM lateral traffic and all cloud link-local/IMDS traffic (inserted at top of FORWARD).
ensure_top_rule FORWARD -i "\${BRIDGE_NAME}" -o "\${BRIDGE_NAME}" -j DROP
ensure_top_rule FORWARD -i "\${BRIDGE_NAME}" -d 169.254.0.0/16 -j DROP

# Isolate host services from microVMs: allow established return traffic and the cache port only, drop everything else.
ensure_top_rule INPUT -i "\${BRIDGE_NAME}" -j DROP
ensure_top_rule INPUT -i "\${BRIDGE_NAME}" -p tcp --dport "\${CACHE_PORT}" -j ACCEPT
ensure_top_rule INPUT -i "\${BRIDGE_NAME}" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
ensure_top_rule INPUT -i "\${BRIDGE_NAME}" -d 169.254.0.0/16 -j DROP

if command -v ip6tables >/dev/null 2>&1 && ip6tables -w 5 -L -n >/dev/null 2>&1; then
  while ip6tables -w 5 -C FORWARD -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP 2>/dev/null; do
    ip6tables -w 5 -D FORWARD -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP
  done
  ip6tables -w 5 -I FORWARD 1 -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP

  while ip6tables -w 5 -C INPUT -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP 2>/dev/null; do
    ip6tables -w 5 -D INPUT -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP
  done
  ip6tables -w 5 -I INPUT 1 -i "\${BRIDGE_NAME}" -d fd00:ec2::254/128 -j DROP
fi
EOF
  chmod 0755 /usr/local/sbin/open-ci-setup-bridge.sh

  cat <<'EOF' > /etc/systemd/system/open-ci-br0.service
[Unit]
Description=Configure open-ci Cloud Hypervisor bridge, NAT, and Zero-IAM IMDS firewall
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/open-ci-setup-bridge.sh

[Install]
WantedBy=multi-user.target
EOF

  if [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
    systemctl enable open-ci-br0.service
    systemctl restart open-ci-br0.service
  else
    /usr/local/sbin/open-ci-setup-bridge.sh
  fi
  log "configured bridge ${BRIDGE_NAME} (${BRIDGE_CIDR}) with NAT, inter-VM isolation, and IMDS firewall"
}

apply_host_tuning() {
  cat <<'EOF' > /etc/sysctl.d/99-open-ci-host.conf
net.ipv4.ip_forward = 1
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
vm.dirty_ratio = 40
vm.dirty_background_ratio = 10
vm.swappiness = 1
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
EOF

  if [[ "${HUGEPAGES_GB}" -gt 0 ]]; then
    local nr_hugepages=$((HUGEPAGES_GB * 512))
    printf 'vm.nr_hugepages = %d\n' "${nr_hugepages}" >> /etc/sysctl.d/99-open-ci-host.conf
    log "reserving ${HUGEPAGES_GB} GiB of 2 MiB hugepages (vm.nr_hugepages=${nr_hugepages})"
  fi

  sysctl -p /etc/sysctl.d/99-open-ci-host.conf >/dev/null

  if [[ -w /sys/kernel/mm/transparent_hugepage/enabled ]]; then
    printf 'always' > /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || true
  fi
  if [[ -w /sys/kernel/mm/transparent_hugepage/defrag ]]; then
    printf 'defer+madvise' > /sys/kernel/mm/transparent_hugepage/defrag 2>/dev/null || true
  fi

  local gov
  for gov in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    if [[ -w "${gov}" ]]; then
      printf 'performance' > "${gov}" 2>/dev/null || true
    fi
  done

  if [[ "${DISABLE_CSTATES}" == "true" ]]; then
    local cstate
    for cstate in /sys/devices/system/cpu/cpu*/cpuidle/state[1-9]*/disable; do
      if [[ -w "${cstate}" ]]; then
        printf '1' > "${cstate}" 2>/dev/null || true
      fi
    done
  fi

  log "applied host sysctl (CUBIC/FQ/64MiB buffers), THP=always, and CPU governor=performance"
}

install_host_packages
verify_kvm
install_cloud_hypervisor
setup_nvme_xfs
apply_host_tuning
install_bridge_service

log "host setup complete. Next: run infra/cloud-hypervisor/build-rootfs.sh with OPEN_CI_BIN=/path/to/open-ci"
