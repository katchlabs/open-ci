#!/usr/bin/env bash
# Builds a lean Ubuntu 24.04 (noble) sparse ext4 rootfs image and extracts a
# direct-boot Linux kernel for open-ci Cloud Hypervisor microVMs.
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "ERROR: build-rootfs.sh must be run as root (try: sudo OPEN_CI_BIN=... $0)" >&2
  exit 1
fi

: "${OPEN_CI_BIN:?OPEN_CI_BIN must point to the compiled Linux open-ci binary}"
if [[ ! -f "${OPEN_CI_BIN}" ]]; then
  echo "ERROR: OPEN_CI_BIN does not exist: ${OPEN_CI_BIN}" >&2
  exit 1
fi

OUTPUT_DIR="${OUTPUT_DIR:-/var/lib/open-ci/images}"
ROOTFS_IMAGE="${ROOTFS_IMAGE:-${OUTPUT_DIR}/ubuntu-24.04-rootfs.ext4}"
KERNEL_OUTPUT="${KERNEL_OUTPUT:-${OUTPUT_DIR}/vmlinux}"
ROOTFS_SIZE="${ROOTFS_SIZE:-64G}"
UBUNTU_SUITE="${UBUNTU_SUITE:-noble}"
KERNEL_PACKAGE="${KERNEL_PACKAGE:-linux-image-kvm}"
RUNNER_VERSION="${RUNNER_VERSION:-2.323.0}"
RUNNER_SHA256="${RUNNER_SHA256:-}"
GUEST_DNS="${GUEST_DNS:-1.1.1.1 8.8.8.8}"

RAW_ARCH="${ARCH:-$(uname -m)}"
case "${RAW_ARCH}" in
  x86_64|amd64|x64)
    DEB_ARCH="amd64"
    RUNNER_ARCH="x64"
    KERNEL_ARCH="x86_64"
    DEFAULT_MIRROR="http://archive.ubuntu.com/ubuntu"
    ;;
  aarch64|arm64)
    DEB_ARCH="arm64"
    RUNNER_ARCH="arm64"
    KERNEL_ARCH="aarch64"
    DEFAULT_MIRROR="http://ports.ubuntu.com/ubuntu-ports"
    ;;
  *)
    echo "ERROR: unsupported ARCH '${RAW_ARCH}' (expected x86_64/amd64/x64 or aarch64/arm64)" >&2
    exit 1
    ;;
esac
UBUNTU_MIRROR="${UBUNTU_MIRROR:-${DEFAULT_MIRROR}}"

HOST_ARCH="$(uname -m)"
if [[ "${HOST_ARCH}" != "${KERNEL_ARCH}" ]]; then
  if [[ ! -e "/proc/sys/fs/binfmt_misc/qemu-${KERNEL_ARCH}" ]]; then
    echo "ERROR: cross-building ${KERNEL_ARCH} rootfs on ${HOST_ARCH} requires qemu-user-static binfmt support; install qemu-user-static or build on a native ${KERNEL_ARCH} host" >&2
    exit 1
  fi
fi

for cmd in chroot curl debootstrap e2fsck mkfs.ext4 mount tar truncate umount; do
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "ERROR: required command '${cmd}' not found (run infra/cloud-hypervisor/setup-host.sh first)" >&2
    exit 1
  fi
done

log() {
  printf 'open-ci-ch-rootfs: %s\n' "$*"
}

mkdir -p "${OUTPUT_DIR}" "$(dirname "${ROOTFS_IMAGE}")" "$(dirname "${KERNEL_OUTPUT}")"

MNT="$(mktemp -d /tmp/open-ci-rootfs-mnt.XXXXXX)"
TMP_IMG="${ROOTFS_IMAGE}.tmp.$$"
TMP_KERNEL="${KERNEL_OUTPUT}.tmp.$$"
TMP_EXTRACT="$(mktemp -d /tmp/open-ci-kernel-extract.XXXXXX)"
FINALIZED=0

cleanup() {
  local sub
  for sub in dev/pts dev sys proc; do
    if mountpoint -q "${MNT}/${sub}" 2>/dev/null; then
      umount "${MNT}/${sub}" 2>/dev/null || umount -lf "${MNT}/${sub}" 2>/dev/null || true
    fi
  done
  if mountpoint -q "${MNT}" 2>/dev/null; then
    umount "${MNT}" 2>/dev/null || umount -lf "${MNT}" 2>/dev/null || true
  fi
  rmdir "${MNT}" 2>/dev/null || true
  rm -rf "${TMP_EXTRACT}" 2>/dev/null || true
  if [[ "${FINALIZED}" -ne 1 ]]; then
    rm -f "${TMP_IMG}" "${TMP_KERNEL}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

is_elf_file() {
  local file="$1"
  local magic
  magic="$(dd if="${file}" bs=1 count=4 status=none 2>/dev/null | od -An -tx1 | tr -d ' \n')"
  [[ "${magic}" == "7f454c46" ]]
}

try_decompress_elf() {
  local img="$1"
  local pattern="$2"
  local out="$3"
  shift 3
  local pos candidate

  candidate="${TMP_EXTRACT}/vmlinux.cand"

  while IFS=: read -r pos _; do
    [[ -n "${pos}" ]] || continue
    rm -f "${candidate}"
    if tail -c +"$((pos + 1))" "${img}" 2>/dev/null \
      | "$@" > "${candidate}" 2>/dev/null; then
      if is_elf_file "${candidate}"; then
        mv -f "${candidate}" "${out}"
        return 0
      fi
    fi
  done < <(LC_ALL=C grep -aobP -- "${pattern}" "${img}" 2>/dev/null || true)

  return 1
}

extract_kernel() {
  local vmlinuz="$1"
  local out="$2"

  if [[ "${KERNEL_ARCH}" == "aarch64" ]]; then
    local gz_magic
    gz_magic="$(dd if="${vmlinuz}" bs=1 count=2 status=none | od -An -tx1 | tr -d ' \n')"
    if [[ "${gz_magic}" == "1f8b" ]]; then
      gzip -dc "${vmlinuz}" > "${out}"
      log "decompressed ARM64 kernel Image to ${KERNEL_OUTPUT}"
    else
      cp "${vmlinuz}" "${out}"
      log "copied raw ARM64 kernel Image to ${KERNEL_OUTPUT}"
    fi
    return 0
  fi

  if is_elf_file "${vmlinuz}"; then
    cp "${vmlinuz}" "${out}"
    log "copied ELF vmlinux directly to ${KERNEL_OUTPUT}"
    return 0
  fi

  if command -v zstd >/dev/null 2>&1 && try_decompress_elf "${vmlinuz}" '\x28\xb5\x2f\xfd' "${out}" zstd -dcq; then
    log "extracted uncompressed ELF vmlinux (zstd) to ${KERNEL_OUTPUT}"
    return 0
  fi
  if command -v gzip >/dev/null 2>&1 && try_decompress_elf "${vmlinuz}" '\x1f\x8b\x08' "${out}" gzip -dc; then
    log "extracted uncompressed ELF vmlinux (gzip) to ${KERNEL_OUTPUT}"
    return 0
  fi
  if command -v xz >/dev/null 2>&1 && try_decompress_elf "${vmlinuz}" '\xfd7zXZ\x00' "${out}" xz -dc; then
    log "extracted uncompressed ELF vmlinux (xz) to ${KERNEL_OUTPUT}"
    return 0
  fi
  if command -v lz4 >/dev/null 2>&1 && try_decompress_elf "${vmlinuz}" '\x02\x21\x4c\x18' "${out}" lz4 -dc; then
    log "extracted uncompressed ELF vmlinux (lz4) to ${KERNEL_OUTPUT}"
    return 0
  fi

  cp "${vmlinuz}" "${out}"
  log "copied bzImage kernel directly to ${KERNEL_OUTPUT} (native Cloud Hypervisor x86_64 bzImage boot)"
}

log "creating ${ROOTFS_SIZE} sparse ext4 image at ${TMP_IMG}"
truncate -s "${ROOTFS_SIZE}" "${TMP_IMG}"
mkfs.ext4 -F -m 0 \
  -O ^has_journal,sparse_super2 \
  -E lazy_itable_init=0,nodiscard \
  -L open-ci-root \
  "${TMP_IMG}" >/dev/null

mount -o loop,noatime,nodiratime "${TMP_IMG}" "${MNT}"

log "bootstrapping Ubuntu ${UBUNTU_SUITE} (${DEB_ARCH}) from ${UBUNTU_MIRROR}"
debootstrap --variant=minbase --arch="${DEB_ARCH}" "${UBUNTU_SUITE}" "${MNT}" "${UBUNTU_MIRROR}"

cat <<EOF > "${MNT}/etc/apt/sources.list"
deb ${UBUNTU_MIRROR} ${UBUNTU_SUITE} main universe
deb ${UBUNTU_MIRROR} ${UBUNTU_SUITE}-updates main universe
deb ${UBUNTU_MIRROR} ${UBUNTU_SUITE}-security main universe
EOF

mkdir -p "${MNT}/etc/dpkg/dpkg.cfg.d" "${MNT}/etc/apt/apt.conf.d"
cat <<'EOF' > "${MNT}/etc/dpkg/dpkg.cfg.d/01-open-ci-speedup"
force-unsafe-io
EOF

cat <<'EOF' > "${MNT}/etc/apt/apt.conf.d/99-open-ci-speedup"
Acquire::Languages "none";
Acquire::GzipIndexes "false";
Acquire::http::Pipeline-Depth "10";
Acquire::Queue-Mode "host";
EOF

# Prevent package postinst scripts from starting daemons inside the build chroot.
cat <<'EOF' > "${MNT}/usr/sbin/policy-rc.d"
#!/bin/sh
exit 101
EOF
chmod 0755 "${MNT}/usr/sbin/policy-rc.d"

rm -f "${MNT}/etc/resolv.conf"
cp /etc/resolv.conf "${MNT}/etc/resolv.conf"

mount --bind /proc "${MNT}/proc"
mount --bind /sys "${MNT}/sys"
mount --bind /dev "${MNT}/dev"
mount --bind /dev/pts "${MNT}/dev/pts"

log "installing guest base packages, toolchain, and kernel (${KERNEL_PACKAGE})"
chroot "${MNT}" /usr/bin/env \
  DEBIAN_FRONTEND=noninteractive \
  KERNEL_PACKAGE="${KERNEL_PACKAGE}" \
  /bin/bash -euo pipefail -c '
    if command -v debconf-set-selections >/dev/null 2>&1; then
      printf "man-db man-db/auto-update boolean false\n" | debconf-set-selections
    fi
    rm -f /var/lib/man-db/auto-update

    apt-get update -qq
    KPKG="${KERNEL_PACKAGE}"
    if ! apt-cache show "${KPKG}" >/dev/null 2>&1; then
      KPKG="linux-image-virtual"
    fi

    apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      curl \
      git \
      iproute2 \
      iputils-ping \
      jq \
      pigz \
      python3 \
      sudo \
      systemd \
      systemd-sysv \
      tar \
      udev \
      unzip \
      zstd \
      "${KPKG}"
  '

VMLINUZ_PATH="$(find "${MNT}/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -n1 || true)"
if [[ -z "${VMLINUZ_PATH}" ]]; then
  echo "ERROR: no /boot/vmlinuz-* kernel found inside built rootfs" >&2
  exit 1
fi
extract_kernel "${VMLINUZ_PATH}" "${TMP_KERNEL}"

cat <<'EOF' > "${MNT}/etc/fstab"
/dev/vda / ext4 rw,noatime,nodiratime,nobarrier,noblock_validity,lazytime,noinit_itable 0 1
EOF

printf 'open-ci-runner\n' > "${MNT}/etc/hostname"
cat <<'EOF' > "${MNT}/etc/hosts"
127.0.0.1 localhost open-ci-runner
::1 localhost ip6-localhost ip6-loopback
EOF

mkdir -p "${MNT}/etc/modules-load.d" "${MNT}/etc/sysctl.d"
cat <<'EOF' > "${MNT}/etc/sysctl.d/99-open-ci.conf"
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

log "installing open-ci agent binary and GitHub Actions runner v${RUNNER_VERSION}"
install -m 0755 "${OPEN_CI_BIN}" "${MNT}/usr/local/bin/open-ci"

install -d -m 0755 "${MNT}/usr/local/sbin"
cat <<'EOF' > "${MNT}/usr/local/sbin/open-ci-microvm-init.sh"
#!/usr/bin/env bash
# Ephemeral Cloud Hypervisor microVM bootstrapper for open-ci.
# Reads the OPENCI01 + big-endian uint64 length seed payload from /dev/vdb,
# executes it via open-ci agent bootstrap, scrubs the payload, and powers off.
set -euo pipefail

SEED_DEV="/dev/vdb"
USERDATA_SCRIPT="/run/open-ci-userdata.sh"
MAX_SEED_BYTES=1048576

scrub_and_poweroff() {
  rm -f "${USERDATA_SCRIPT}" 2>/dev/null || true
  sync 2>/dev/null || true
  poweroff -f
}
trap scrub_and_poweroff EXIT

configure_network() {
  local ip_arg="" token
  for token in $(cat /proc/cmdline); do
    case "${token}" in
      ip=*) ip_arg="${token#ip=}" ;;
    esac
  done
  [[ -n "${ip_arg}" ]] || return 0

  IFS=":" read -r guest_ip _ gw_ip netmask _ _ _ <<< "${ip_arg}"
  [[ -n "${guest_ip}" && -n "${gw_ip}" ]] || return 0

  local prefix=0 octet mask_valid=1
  if [[ "${netmask}" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    for octet in "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[4]}"; do
      case "${octet}" in
        255) prefix=$((prefix + 8)) ;;
        254) prefix=$((prefix + 7)) ;;
        252) prefix=$((prefix + 6)) ;;
        248) prefix=$((prefix + 5)) ;;
        240) prefix=$((prefix + 4)) ;;
        224) prefix=$((prefix + 3)) ;;
        192) prefix=$((prefix + 2)) ;;
        128) prefix=$((prefix + 1)) ;;
        0) ;;
        *) mask_valid=0 ;;
      esac
    done
  else
    mask_valid=0
  fi
  if (( mask_valid == 0 || prefix < 8 || prefix > 30 )); then
    prefix=16
  fi

  local iface="" i
  for i in $(seq 1 100); do
    iface="$(ip -o link show | awk -F': ' '$2 != "lo" {print $2; exit}')"
    iface="${iface%%@*}"
    [[ -n "${iface}" ]] && break
    sleep 0.05
  done
  if [[ -n "${iface}" ]]; then
    ip link set "${iface}" up
    ip addr replace "${guest_ip}/${prefix}" dev "${iface}"
    ip route replace default via "${gw_ip}" dev "${iface}"
  fi
}

tune_guest() {
  if [[ -w /sys/kernel/mm/transparent_hugepage/enabled ]]; then
    printf 'always' > /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || true
  fi
  if [[ -w /sys/kernel/mm/transparent_hugepage/defrag ]]; then
    printf 'defer+madvise' > /sys/kernel/mm/transparent_hugepage/defrag 2>/dev/null || true
  fi

  local qdir="/sys/block/vda/queue"
  if [[ -d "${qdir}" ]]; then
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
  fi
}

wait_for_seed_device() {
  local i
  for i in $(seq 1 100); do
    if [[ -b "${SEED_DEV}" ]]; then
      return 0
    fi
    sleep 0.05
  done
  echo "open-ci-microvm: seed block device ${SEED_DEV} not present" >&2
  return 1
}

extract_seed_payload() {
  local magic_hex hex_len payload_len

  # 4f50454e43493031 == ASCII "OPENCI01"
  magic_hex="$(dd if="${SEED_DEV}" bs=4096 count=8 iflag=count_bytes status=none | od -An -tx1 | tr -d ' \n')"
  if [[ "${magic_hex}" != "4f50454e43493031" ]]; then
    echo "open-ci-microvm: invalid seed header magic on ${SEED_DEV} (got '${magic_hex}')" >&2
    return 1
  fi

  hex_len="$(dd if="${SEED_DEV}" bs=4096 skip=8 count=8 iflag=skip_bytes,count_bytes status=none | od -An -tx1 | tr -d ' \n')"
  if [[ ! "${hex_len}" =~ ^[0-9a-fA-F]{16}$ ]]; then
    echo "open-ci-microvm: malformed seed payload length on ${SEED_DEV}" >&2
    return 1
  fi

  payload_len=$((16#${hex_len}))
  if (( payload_len <= 0 || payload_len > MAX_SEED_BYTES )); then
    echo "open-ci-microvm: seed payload length ${payload_len} out of bounds (1..=${MAX_SEED_BYTES})" >&2
    return 1
  fi

  umask 077
  dd if="${SEED_DEV}" of="${USERDATA_SCRIPT}" \
    bs=4096 skip=16 count="${payload_len}" \
    iflag=skip_bytes,count_bytes status=none
  chmod 0700 "${USERDATA_SCRIPT}"
}

configure_network
tune_guest
wait_for_seed_device
extract_seed_payload

OPEN_CI_AGENT_NO_POWEROFF=1 "${USERDATA_SCRIPT}"
EOF
chmod 0755 "${MNT}/usr/local/sbin/open-ci-microvm-init.sh"

cat <<'EOF' > "${MNT}/etc/systemd/system/open-ci-microvm.service"
[Unit]
Description=open-ci Cloud Hypervisor ephemeral runner bootstrap
After=network.target
Wants=network.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/open-ci-microvm-init.sh
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
EOF

chroot "${MNT}" /usr/bin/env \
  DEBIAN_FRONTEND=noninteractive \
  RUNNER_ARCH="${RUNNER_ARCH}" \
  RUNNER_VERSION="${RUNNER_VERSION}" \
  RUNNER_SHA256="${RUNNER_SHA256}" \
  /bin/bash -euo pipefail -c '
    if ! id -u runner >/dev/null 2>&1; then
      useradd --create-home --shell /bin/bash runner
    fi

    cat <<SUDOEOF > /etc/sudoers.d/90-open-ci-runner
runner ALL=(ALL) NOPASSWD:ALL
SUDOEOF
    chmod 0440 /etc/sudoers.d/90-open-ci-runner
    visudo -cf /etc/sudoers.d/90-open-ci-runner >/dev/null

    RUNNER_DIR="/opt/actions-runner"
    ARCHIVE="actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz"
    DOWNLOAD_URL="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/${ARCHIVE}"

    mkdir -p "${RUNNER_DIR}/_work"
    curl -fsSL --retry 3 --retry-delay 2 "${DOWNLOAD_URL}" -o "/tmp/${ARCHIVE}"
    if [[ -n "${RUNNER_SHA256}" ]]; then
      printf "%s  /tmp/%s\n" "${RUNNER_SHA256}" "${ARCHIVE}" | sha256sum -c -
    fi
    tar -xzf "/tmp/${ARCHIVE}" -C "${RUNNER_DIR}"
    rm -f "/tmp/${ARCHIVE}"

    "${RUNNER_DIR}/bin/installdependencies.sh"
    chown -R runner:runner "${RUNNER_DIR}"

    systemctl enable open-ci-microvm.service
    systemctl mask \
      apt-daily.timer \
      apt-daily.service \
      apt-daily-upgrade.timer \
      apt-daily-upgrade.service \
      unattended-upgrades.service \
      man-db.timer \
      man-db.service \
      systemd-networkd-wait-online.service \
      e2scrub_all.timer \
      e2scrub_all.service \
      e2scrub_reap.service 2>/dev/null || true

    rm -f /var/lib/man-db/auto-update
    apt-get clean
    rm -rf /tmp/* /var/tmp/*
    rm -f /etc/ssh/ssh_host_*
    truncate -s 0 /etc/machine-id
    rm -f /var/lib/dbus/machine-id
  '

rm -f "${MNT}/usr/sbin/policy-rc.d"

# Write static guest DNS so resolution works instantaneously without systemd-resolved.
rm -f "${MNT}/etc/resolv.conf"
: > "${MNT}/etc/resolv.conf"
read -r -a guest_dns_arr <<< "${GUEST_DNS}"
for ns in "${guest_dns_arr[@]}"; do
  printf 'nameserver %s\n' "${ns}" >> "${MNT}/etc/resolv.conf"
done
chmod 0644 "${MNT}/etc/resolv.conf"

# Cleanly unmount chroot bind mounts and rootfs before filesystem verification.
for sub in dev/pts dev sys proc; do
  if mountpoint -q "${MNT}/${sub}"; then
    umount "${MNT}/${sub}"
  fi
done
umount "${MNT}"

log "checking ext4 filesystem integrity"
fsck_rc=0
e2fsck -fy "${TMP_IMG}" >/dev/null || fsck_rc=$?
if [[ "${fsck_rc}" -ge 2 ]]; then
  echo "ERROR: e2fsck failed on ${TMP_IMG} with exit code ${fsck_rc}" >&2
  exit "${fsck_rc}"
fi

chmod 0644 "${TMP_IMG}" "${TMP_KERNEL}"
mv -f "${TMP_IMG}" "${ROOTFS_IMAGE}"
mv -f "${TMP_KERNEL}" "${KERNEL_OUTPUT}"
FINALIZED=1

log "built rootfs image: ${ROOTFS_IMAGE} (virtual size ${ROOTFS_SIZE}, actual $(du -h "${ROOTFS_IMAGE}" | awk '{print $1}'))"
log "extracted kernel:   ${KERNEL_OUTPUT}"
