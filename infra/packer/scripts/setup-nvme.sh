#!/usr/bin/env bash
# Auto-detects unmounted local ephemeral NVMe SSDs (AWS Instance Store / GCP Local SSD),
# formats a single drive or RAID0 stripes 2+ drives, mounts at /mnt/nvme, prepares
# Docker/workspace/tmp directories, and points Docker data-root at /mnt/nvme/docker.
set -euo pipefail

MOUNT_POINT="/mnt/nvme"
BACKING_MOUNT="/mnt/nvme-backing"
NVME_PAGE_CACHE="${OPEN_CI_NVME_PAGE_CACHE:-auto}"
LOOP_IMAGE_SIZE="${OPEN_CI_NVME_IMAGE_SIZE:-}"
DISABLE_CSTATES="${OPEN_CI_DISABLE_CSTATES:-false}"
RUNNER_WORK_LINK="/opt/actions-runner/_work"

host_mem_gib() {
  local mem_kb
  mem_kb="$(awk '/^MemTotal:/ { print $2; exit }' /proc/meminfo 2>/dev/null || echo 0)"
  if [[ "${mem_kb}" =~ ^[0-9]+$ ]] && [[ "${mem_kb}" -gt 0 ]]; then
    echo $(( mem_kb / 1048576 ))
  else
    echo 0
  fi
}

HOST_MEM_GIB="$(host_mem_gib)"

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
  if [[ -w "${qdir}/rq_affinity" ]]; then
    printf '1' > "${qdir}/rq_affinity" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/add_random" ]]; then
    printf '0' > "${qdir}/add_random" 2>/dev/null || true
  fi
  if [[ -w "${qdir}/iostats" ]]; then
    printf '0' > "${qdir}/iostats" 2>/dev/null || true
  fi
}

tune_ephemeral_host() {
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

  local root_fstype
  root_fstype="$(findmnt -n -o FSTYPE / 2>/dev/null || true)"
  if [[ "${root_fstype}" == "ext4" ]]; then
    mount -o remount,noatime,nobarrier,commit=60 / 2>/dev/null || true
  fi

  if [[ "${HOST_MEM_GIB}" -ge 14 ]]; then
    sysctl -q -w \
      vm.dirty_ratio=60 \
      vm.dirty_background_ratio=30 \
      vm.dirty_expire_centisecs=60000 \
      vm.dirty_writeback_centisecs=6000 \
      net.core.rmem_default=2097152 \
      net.core.wmem_default=2097152 2>/dev/null || true
  else
    sysctl -q -w \
      vm.dirty_ratio=40 \
      vm.dirty_background_ratio=10 \
      net.core.rmem_default=262144 \
      net.core.wmem_default=262144 2>/dev/null || true
  fi
}

ensure_directories() {
  mkdir -p "${MOUNT_POINT}/docker" "${MOUNT_POINT}/work/_temp" "${MOUNT_POINT}/tmp"
  chmod 0755 "${MOUNT_POINT}" "${MOUNT_POINT}/docker"
  chmod 1777 "${MOUNT_POINT}/tmp"
  if id -u runner >/dev/null 2>&1; then
    chown runner:runner "${MOUNT_POINT}/work" "${MOUNT_POINT}/work/_temp"
  fi
  if [[ -d /opt/actions-runner ]]; then
    if [[ -d "${RUNNER_WORK_LINK}" && ! -L "${RUNNER_WORK_LINK}" ]]; then
      rmdir "${RUNNER_WORK_LINK}" 2>/dev/null || true
    fi
    if [[ ! -e "${RUNNER_WORK_LINK}" || -L "${RUNNER_WORK_LINK}" ]]; then
      ln -sfn "${MOUNT_POINT}/work" "${RUNNER_WORK_LINK}"
      if id -u runner >/dev/null 2>&1; then
        chown -h runner:runner "${RUNNER_WORK_LINK}"
      fi
    fi
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

tune_ephemeral_host

if mountpoint -q "${MOUNT_POINT}"; then
  MOUNT_SRC="$(findmnt -n -o SOURCE "${MOUNT_POINT}" 2>/dev/null || true)"
  if [[ -n "${MOUNT_SRC}" ]]; then
    tune_block_device "${MOUNT_SRC}"
  fi
  ensure_directories
  configure_docker_nvme_root
  exit 0
fi

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

NVME_DEVS=()
while IFS= read -r dev; do
  [[ -n "${dev}" ]] || continue
  [[ "${dev}" =~ ^/dev/nvme[0-9]+n[0-9]+$ ]] || continue
  [[ "${dev}" != "${ROOT_DISK}" ]] || continue

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

  NVME_DEVS+=("${dev}")
done < <(lsblk -dnpo NAME,TYPE 2>/dev/null | awk '$2 == "disk" { print $1 }')

if [[ "${#NVME_DEVS[@]}" -eq 0 ]]; then
  echo "open-ci-nvme: no unmounted local ephemeral NVMe drives found; continuing with root volume"
  exit 0
fi

if [[ "${#NVME_DEVS[@]}" -eq 1 ]]; then
  TARGET_DEV="${NVME_DEVS[0]}"
  echo "open-ci-nvme: formatting single NVMe device ${TARGET_DEV}"
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
fi

for dev in "${NVME_DEVS[@]}"; do
  tune_block_device "${dev}"
done
if [[ "${TARGET_DEV}" != "${NVME_DEVS[0]}" ]]; then
  tune_block_device "${TARGET_DEV}"
fi

USE_PAGE_CACHE=0
case "${NVME_PAGE_CACHE}" in
  true)
    USE_PAGE_CACHE=1
    ;;
  auto)
    if [[ "${HOST_MEM_GIB}" -ge 14 ]]; then
      USE_PAGE_CACHE=1
    fi
    ;;
  false)
    USE_PAGE_CACHE=0
    ;;
  *)
    echo "open-ci-nvme: invalid OPEN_CI_NVME_PAGE_CACHE='${NVME_PAGE_CACHE}' (expected auto|true|false)" >&2
    exit 1
    ;;
esac

mkdir -p "${MOUNT_POINT}"
if [[ "${USE_PAGE_CACHE}" -eq 1 ]] && command -v mkfs.xfs >/dev/null 2>&1 && command -v losetup >/dev/null 2>&1; then
  mkfs.xfs -f -m reflink=1,crc=1 "${TARGET_DEV}" >/dev/null
  mkdir -p "${BACKING_MOUNT}"
  mount -t xfs -o noatime,nodiratime,logbufs=8,logbsize=256k,allocsize=64m "${TARGET_DEV}" "${BACKING_MOUNT}"

  if [[ -n "${LOOP_IMAGE_SIZE}" ]]; then
    RESOLVED_LOOP_SIZE="${LOOP_IMAGE_SIZE}"
  else
    AVAIL_GIB="$(df -BG --output=avail "${BACKING_MOUNT}" | tail -n1 | tr -dc '0-9')"
    CALC_GIB=$(( AVAIL_GIB * 85 / 100 ))
    if [[ "${CALC_GIB}" -lt 4 ]]; then
      CALC_GIB=4
    fi
    RESOLVED_LOOP_SIZE="${CALC_GIB}G"
  fi

  LOOP_IMG="${BACKING_MOUNT}/runner-work.img"
  truncate -s "${RESOLVED_LOOP_SIZE}" "${LOOP_IMG}"
  mkfs.ext4 -F -m 0 -b 4096 -O ^has_journal,sparse_super2 -E lazy_itable_init=1,nodiscard "${LOOP_IMG}" >/dev/null
  LOOP_DEV="$(losetup --find --show --sector-size 4096 "${LOOP_IMG}")"
  tune_block_device "${LOOP_DEV}"
  mount -o noatime,nodiratime,nobarrier,noblock_validity,lazytime,noinit_itable "${LOOP_DEV}" "${MOUNT_POINT}"
  echo "open-ci-nvme: mounted page-cache-accelerated ${LOOP_DEV} (${RESOLVED_LOOP_SIZE}, backed by XFS ${TARGET_DEV}) at ${MOUNT_POINT}"
else
  mkfs.ext4 -F -m 0 -O ^has_journal,sparse_super2 -E lazy_itable_init=1,nodiscard "${TARGET_DEV}"
  mount -o noatime,nodiratime,nobarrier,noblock_validity,lazytime,noinit_itable "${TARGET_DEV}" "${MOUNT_POINT}"
  echo "open-ci-nvme: mounted ${TARGET_DEV} at ${MOUNT_POINT}"
fi

ensure_directories
configure_docker_nvme_root
