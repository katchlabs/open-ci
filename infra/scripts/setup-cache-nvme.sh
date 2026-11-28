#!/usr/bin/env bash
# Formats and mounts an ephemeral local NVMe disk (AWS Instance Store / GCP Local SSD) as the
# open-ci cache directory: journal-less ext4 (`-O ^has_journal -m 0`) mounted `noatime` without
# online `discard`, so concurrent multi-GB cache saves never queue behind ext4 journal commits
# or synchronous TRIM. Idempotent and boot-safe: re-run it on every boot via --install-unit.
set -euo pipefail

DEVICE="${OPEN_CI_CACHE_NVME_DEVICE:-}"
MOUNT_POINT="${OPEN_CI_CACHE_NVME_MOUNT:-/var/lib/open-ci-cache}"
OWNER="${OPEN_CI_CACHE_NVME_OWNER:-open-ci}"
FS_LABEL="open-ci-cache"
INSTALL_UNIT="false"
FORCE="false"
INSTALLED_SCRIPT="/usr/local/sbin/open-ci-setup-cache-nvme.sh"
UNIT_PATH="/etc/systemd/system/open-ci-cache-nvme.service"

usage() {
  cat <<'EOF'
Usage: sudo infra/scripts/setup-cache-nvme.sh --device <block device> [options]

Formats <device> as journal-less ext4 (mkfs.ext4 -F -O ^has_journal -m 0) when it has no
open-ci-cache filesystem yet, then mounts it with -o noatime (no discard) at the cache directory.
Everything on the device, including the cache index, is disposable cache state.

Options:
  --device <path>       Local NVMe block device; prefer a stable /dev/disk/by-id/... path
                        (env: OPEN_CI_CACHE_NVME_DEVICE)
  --mount-point <dir>   Cache directory to mount over (default: /var/lib/open-ci-cache)
  --owner <user>        Owner of the mounted directory (default: open-ci)
  --install-unit        Install this script and an open-ci-cache-nvme.service oneshot that
                        re-runs it on every boot before open-ci.service / open-ci-cache.service
  --force               Reformat a device holding a foreign filesystem, or mount over a
                        non-empty cache directory
  -h, --help            Show this help message
EOF
}

die() {
  echo "open-ci-cache-nvme: ERROR: $*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --device) DEVICE="$2"; shift 2 ;;
    --mount-point) MOUNT_POINT="$2"; shift 2 ;;
    --owner) OWNER="$2"; shift 2 ;;
    --install-unit) INSTALL_UNIT="true"; shift ;;
    --force) FORCE="true"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "ERROR: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ "${EUID}" -eq 0 ]] || die "must be run as root"
[[ -n "${DEVICE}" ]] || { usage >&2; exit 2; }
[[ "${MOUNT_POINT}" == /* ]] || die "--mount-point must be an absolute path"

install_unit() {
  local device_unit
  if [[ "$(readlink -f "${BASH_SOURCE[0]}")" != "${INSTALLED_SCRIPT}" ]]; then
    install -m 0755 "${BASH_SOURCE[0]}" "${INSTALLED_SCRIPT}"
  fi
  device_unit="$(systemd-escape --path --suffix=device "${DEVICE}")"
  cat <<EOF > "${UNIT_PATH}"
[Unit]
Description=open-ci ephemeral NVMe cache disk (journal-less ext4, noatime, no discard)
Wants=${device_unit}
After=local-fs.target ${device_unit}
Before=open-ci.service open-ci-cache.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=${INSTALLED_SCRIPT} --device ${DEVICE} --mount-point ${MOUNT_POINT} --owner ${OWNER}

[Install]
WantedBy=multi-user.target open-ci.service open-ci-cache.service
EOF
  chmod 0644 "${UNIT_PATH}"
  if [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
    systemctl enable open-ci-cache-nvme.service
  fi
  echo "open-ci-cache-nvme: installed ${UNIT_PATH} (runs ${INSTALLED_SCRIPT} on every boot)"
}

root_disk() {
  local root_src root_real parent
  root_src="$(findmnt -n -o SOURCE / 2>/dev/null || true)"
  [[ -n "${root_src}" ]] || return 0
  root_real="$(readlink -f "${root_src}" 2>/dev/null || echo "${root_src}")"
  parent="$(lsblk -no PKNAME "${root_real}" 2>/dev/null | head -n1 || true)"
  if [[ -n "${parent}" ]]; then
    echo "/dev/${parent}"
  else
    echo "${root_real}"
  fi
}

warn_on_slow_mount() {
  local options
  options="$(findmnt -n -o OPTIONS "${MOUNT_POINT}" 2>/dev/null || true)"
  if [[ ",${options}," == *",discard,"* ]]; then
    echo "open-ci-cache-nvme: WARNING: ${MOUNT_POINT} is mounted with online discard; remount without it" >&2
  fi
  if dumpe2fs -h "${DEV_REAL}" 2>/dev/null | grep -Eq '^Filesystem features:.*has_journal'; then
    echo "open-ci-cache-nvme: WARNING: ${DEV_REAL} has an ext4 journal; reformat with --force to drop it" >&2
  fi
}

[[ -b "${DEVICE}" ]] || die "${DEVICE} is not a block device"
DEV_REAL="$(readlink -f "${DEVICE}")"
[[ "${DEV_REAL}" != "$(root_disk)" ]] || die "${DEVICE} is the root disk"
[[ "$(lsblk -nr "${DEV_REAL}" | wc -l)" -eq 1 ]] || die "${DEVICE} has partitions or holders; pass a whole, unused disk"

if [[ "${INSTALL_UNIT}" == "true" ]]; then
  install_unit
fi

if mountpoint -q "${MOUNT_POINT}"; then
  MOUNTED_SRC="$(readlink -f "$(findmnt -n -o SOURCE "${MOUNT_POINT}")")"
  [[ "${MOUNTED_SRC}" == "${DEV_REAL}" ]] || die "${MOUNT_POINT} is already mounted from ${MOUNTED_SRC}"
  warn_on_slow_mount
  echo "open-ci-cache-nvme: ${DEV_REAL} already mounted at ${MOUNT_POINT}"
  exit 0
fi

DEVICE_MOUNTS="$(lsblk -nro MOUNTPOINTS "${DEV_REAL}" 2>/dev/null | sed '/^$/d' || true)"
[[ -z "${DEVICE_MOUNTS}" ]] || die "${DEVICE} is already mounted at ${DEVICE_MOUNTS}"

mkdir -p "${MOUNT_POINT}"
if [[ -n "$(ls -A "${MOUNT_POINT}")" && "${FORCE}" != "true" ]]; then
  die "${MOUNT_POINT} is not empty; stop open-ci and move its contents aside, or pass --force to mount over them"
fi

FS_TYPE="$(blkid -o value -s TYPE "${DEV_REAL}" 2>/dev/null || true)"
CURRENT_LABEL="$(blkid -o value -s LABEL "${DEV_REAL}" 2>/dev/null || true)"
NEEDS_FORMAT="false"
if [[ "${FS_TYPE}" == "ext4" && "${CURRENT_LABEL}" == "${FS_LABEL}" && "${FORCE}" != "true" ]]; then
  FSCK_STATUS=0
  e2fsck -p "${DEV_REAL}" >/dev/null 2>&1 || FSCK_STATUS=$?
  if [[ "${FSCK_STATUS}" -ge 4 ]]; then
    echo "open-ci-cache-nvme: e2fsck could not repair ${DEV_REAL} (status ${FSCK_STATUS}); discarding cache"
    NEEDS_FORMAT="true"
  else
    echo "open-ci-cache-nvme: reusing existing ${FS_LABEL} filesystem on ${DEV_REAL}"
  fi
elif [[ -z "${FS_TYPE}" || "${FORCE}" == "true" ]]; then
  NEEDS_FORMAT="true"
else
  die "${DEVICE} holds a ${FS_TYPE} filesystem (label '${CURRENT_LABEL}'); pass --force to erase it"
fi

if [[ "${NEEDS_FORMAT}" == "true" ]]; then
  echo "open-ci-cache-nvme: formatting ${DEV_REAL} as journal-less ext4"
  mkfs.ext4 -F -O ^has_journal -m 0 -L "${FS_LABEL}" "${DEV_REAL}"
fi

mount -t ext4 -o noatime "${DEV_REAL}" "${MOUNT_POINT}"
if id -u "${OWNER}" >/dev/null 2>&1; then
  chown "${OWNER}:${OWNER}" "${MOUNT_POINT}"
fi
chmod 0700 "${MOUNT_POINT}"
warn_on_slow_mount
echo "open-ci-cache-nvme: mounted ${DEV_REAL} at ${MOUNT_POINT} (ext4 ^has_journal, noatime, no discard)"
