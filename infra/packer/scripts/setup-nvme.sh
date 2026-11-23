#!/usr/bin/env bash
# Auto-detects unmounted local ephemeral NVMe SSDs (AWS Instance Store / GCP Local SSD),
# formats a single drive or RAID0 stripes 2+ drives, mounts at /mnt/nvme, prepares
# Docker/workspace/tmp directories, and points Docker data-root at /mnt/nvme/docker.
set -euo pipefail

MOUNT_POINT="/mnt/nvme"

ensure_directories() {
  mkdir -p "${MOUNT_POINT}/docker" "${MOUNT_POINT}/work" "${MOUNT_POINT}/tmp"
  chmod 0755 "${MOUNT_POINT}" "${MOUNT_POINT}/docker"
  chmod 1777 "${MOUNT_POINT}/tmp"
  if id -u runner >/dev/null 2>&1; then
    chown runner:runner "${MOUNT_POINT}/work"
  fi
}

configure_docker_nvme_root() {
  if [[ -f /etc/docker/daemon.json ]] && command -v jq >/dev/null 2>&1; then
    local tmp_file
    tmp_file="$(mktemp /etc/docker/daemon.json.XXXXXX)"
    jq '. + {"data-root": "/mnt/nvme/docker"}' /etc/docker/daemon.json > "${tmp_file}"
    chmod 0644 "${tmp_file}"
    mv "${tmp_file}" /etc/docker/daemon.json
  fi
}

# Idempotent exit if already mounted.
if mountpoint -q "${MOUNT_POINT}"; then
  ensure_directories
  configure_docker_nvme_root
  exit 0
fi

# Resolve the underlying block device backing "/" so we never touch the root disk.
ROOT_SRC="$(findmnt -n -o SOURCE / 2>/dev/null || true)"
ROOT_DISK=""
if [[ -n "${ROOT_SRC}" ]]; then
  ROOT_REAL="$(readlink -f "${ROOT_SRC}" 2>/dev/null || echo "${ROOT_SRC}")"
  PARENT_NAME="$(lsblk -no PKNAME "${ROOT_REAL}" 2>/dev/null | head -n1 || true)"
  if [[ -n "${PARENT_NAME}" ]]; then
    ROOT_DISK="/dev/${PARENT_NAME}"
  else
    ROOT_DISK="${ROOT_REAL}"
  fi
fi

# Collect raw, unmounted, non-root local NVMe ephemeral disks.
NVME_DEVS=()
while IFS= read -r dev; do
  [[ -n "${dev}" ]] || continue
  [[ "${dev}" =~ ^/dev/nvme[0-9]+n[0-9]+$ ]] || continue
  [[ "${dev}" != "${ROOT_DISK}" ]] || continue

  # Skip disks that already contain partitions, LVM, or RAID children.
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

  # Safeguard: never format persistent network-attached cloud volumes.
  model="$(lsblk -dnro MODEL "${dev}" 2>/dev/null || true)"
  if [[ "${model}" == *"Elastic Block Store"* || "${model}" == *"PersistentDisk"* ]]; then
    continue
  fi

  NVME_DEVS+=("${dev}")
done < <(lsblk -dnpo NAME,TYPE 2>/dev/null | awk '$2 == "disk" { print $1 }')

if [[ "${#NVME_DEVS[@]}" -eq 0 ]]; then
  echo "open-ci-nvme: no unmounted local ephemeral NVMe drives found; continuing with root volume"
  exit 0
fi

if [[ "${#NVME_DEVS[@]}" -eq 1 ]]; then
  TARGET_DEV="${NVME_DEVS[0]}"
  echo "open-ci-nvme: formatting single NVMe device ${TARGET_DEV}"
  mkfs.ext4 -F -m 0 -O ^has_journal -E lazy_itable_init=0,lazy_journal_init=0 "${TARGET_DEV}"
else
  TARGET_DEV="/dev/md0"
  echo "open-ci-nvme: creating RAID0 array ${TARGET_DEV} across ${#NVME_DEVS[@]} NVMe devices: ${NVME_DEVS[*]}"
  mdadm --create --verbose "${TARGET_DEV}" \
    --level=0 \
    --raid-devices="${#NVME_DEVS[@]}" \
    --chunk=256 \
    --force \
    "${NVME_DEVS[@]}"
  udevadm settle || true
  mkfs.ext4 -F -m 0 -O ^has_journal -E lazy_itable_init=0,lazy_journal_init=0 "${TARGET_DEV}"
fi

mkdir -p "${MOUNT_POINT}"
mount -o defaults,noatime,nodiratime "${TARGET_DEV}" "${MOUNT_POINT}"

ensure_directories
configure_docker_nvme_root
echo "open-ci-nvme: mounted ${TARGET_DEV} at ${MOUNT_POINT}"
