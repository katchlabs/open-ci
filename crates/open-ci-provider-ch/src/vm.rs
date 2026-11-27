//! Machine shape parsing, deterministic network slot allocation, `seed.raw`
//! payload framing, and `meta.json` persistence for Cloud Hypervisor micro-VMs.

use open_ci_provider::ProviderError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Magic header prefixed to `<vm_dir>/seed.raw` for `open-ci-microvm-init`.
pub const SEED_MAGIC: &[u8; 8] = b"OPENCI01";
/// Block sector alignment for `seed.raw` attached as read-only `/dev/vdb`.
pub const SEED_SECTOR_SIZE: usize = 512;
/// Minimum `seed.raw` image size (4 KiB page) so Linux `virtio_blk` always exposes non-zero blocks.
pub const SEED_MIN_BYTES: usize = 4096;
/// Maximum Linux interface name length (`IFNAMSIZ - 1`).
pub const MAX_IFNAMSIZ: usize = 15;

/// Allocated per-VM IPv4, MAC, and TAP network parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmNetSlot {
    pub slot_index: u32,
    pub guest_ip: String,
    pub gateway_ip: String,
    pub netmask: String,
    pub mac_addr: String,
    pub tap_name: String,
}

/// Persisted metadata stored at `<state_dir>/<worker_name>/meta.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmMetadata {
    pub worker_name: String,
    pub pool_id: String,
    pub zone: String,
    pub pid: u32,
    pub socket_path: PathBuf,
    pub tap_name: String,
    pub guest_ip: String,
    pub gateway_ip: String,
    pub mac_addr: String,
    pub rootfs_path: PathBuf,
    pub seed_path: PathBuf,
    pub expires_at_unix: u64,
}

/// Parses machine types such as `ch-4c-16g`, `ch-8c-32g`, or `4c-16g` into `(vcpus, memory_mib)`,
/// falling back cleanly to the configured defaults for any missing or out-of-range dimension.
pub fn parse_machine_shape(
    machine_type: &str,
    default_vcpus: u32,
    default_memory_mib: u64,
) -> (u32, u64) {
    let mut vcpus = None;
    let mut memory_mib = None;

    for token in machine_type.split('-') {
        if let Some(vcpu_text) = token.strip_suffix(['c', 'C'])
            && let Ok(parsed) = vcpu_text.parse::<u32>()
            && (1..=128).contains(&parsed)
        {
            vcpus = Some(parsed);
        } else if let Some(gigabytes_text) = token.strip_suffix(['g', 'G'])
            && let Ok(parsed_gib) = gigabytes_text.parse::<u64>()
            && let Some(mib) = parsed_gib.checked_mul(1024)
            && (512..=524_288).contains(&mib)
        {
            memory_mib = Some(mib);
        } else if let Some(megabytes_text) = token.strip_suffix(['m', 'M'])
            && let Ok(mib) = megabytes_text.parse::<u64>()
            && (512..=524_288).contains(&mib)
        {
            memory_mib = Some(mib);
        }
    }

    (
        vcpus.unwrap_or(default_vcpus),
        memory_mib.unwrap_or(default_memory_mib),
    )
}

/// Validates a worker name before using it as a directory path segment.
pub fn validate_worker_name(worker_name: &str) -> Result<(), ProviderError> {
    if worker_name.is_empty()
        || worker_name.len() > 128
        || worker_name == "."
        || worker_name == ".."
        || !worker_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ProviderError::InvalidConfig(format!(
            "unsafe worker name `{worker_name}`"
        )));
    }
    Ok(())
}

/// Builds the guest kernel command line with static Linux `ip=` autoconfiguration.
pub fn build_kernel_cmdline(base_cmdline: &str, slot: &VmNetSlot, worker_name: &str) -> String {
    format!(
        "{base_cmdline} ip={}::{}:{}:{worker_name}:eth0:off",
        slot.guest_ip, slot.gateway_ip, slot.netmask
    )
}

/// Encodes `user_data` into a 512-byte sector-aligned `seed.raw` image prefixed with
/// `OPENCI01` and an 8-byte big-endian payload byte length.
pub fn encode_seed_image(user_data: &str) -> Result<Vec<u8>, ProviderError> {
    let payload = user_data.as_bytes();
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| ProviderError::InvalidConfig("seed payload too large".to_owned()))?;
    let header_len = SEED_MAGIC.len() + 8;
    let unpadded_len = header_len
        .checked_add(payload.len())
        .ok_or_else(|| ProviderError::InvalidConfig("seed payload too large".to_owned()))?;
    let padded_len = unpadded_len
        .max(SEED_MIN_BYTES)
        .div_ceil(SEED_MIN_BYTES)
        .checked_mul(SEED_MIN_BYTES)
        .ok_or_else(|| ProviderError::InvalidConfig("seed payload too large".to_owned()))?;

    let mut image = vec![0_u8; padded_len];
    image[..8].copy_from_slice(SEED_MAGIC);
    image[8..16].copy_from_slice(&payload_len.to_be_bytes());
    image[16..16 + payload.len()].copy_from_slice(payload);
    Ok(image)
}

/// Decodes a `seed.raw` image back into the original UTF-8 `user_data` string.
pub fn decode_seed_image(raw: &[u8]) -> Result<String, ProviderError> {
    if raw.len() < 16 || &raw[..8] != SEED_MAGIC {
        return Err(ProviderError::InvalidConfig(
            "invalid seed.raw header magic".to_owned(),
        ));
    }
    let len_bytes: [u8; 8] = raw[8..16]
        .try_into()
        .map_err(|_| ProviderError::InvalidConfig("truncated seed.raw length".to_owned()))?;
    let payload_len = usize::try_from(u64::from_be_bytes(len_bytes))
        .map_err(|_| ProviderError::InvalidConfig("seed.raw length overflow".to_owned()))?;
    let payload_end = 16_usize
        .checked_add(payload_len)
        .ok_or_else(|| ProviderError::InvalidConfig("seed.raw length overflow".to_owned()))?;
    if payload_end > raw.len() {
        return Err(ProviderError::InvalidConfig(
            "seed.raw payload truncated".to_owned(),
        ));
    }
    String::from_utf8(raw[16..payload_end].to_vec())
        .map_err(|_| ProviderError::InvalidConfig("seed.raw payload is not valid UTF-8".to_owned()))
}

/// Writes an encoded `seed.raw` image to disk with `0o600` permissions on Unix.
pub fn write_seed_image(path: &Path, user_data: &str) -> Result<(), ProviderError> {
    use std::io::Write as _;
    let bytes = encode_seed_image(user_data)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|err| ProviderError::Transient(format!("failed opening seed.raw: {err}")))?;
    file.write_all(&bytes)
        .map_err(|err| ProviderError::Transient(format!("failed writing seed.raw: {err}")))
}

/// Deterministically allocates an unused IPv4, MAC, and Linux TAP slot (`oci-<hex>`, `<= 15` chars)
/// by scanning active `meta.json` records under `state_dir`.
///
/// The gateway occupies base `.0.1`; guest VMs are allocated from `.0.2` through `.254.254`.
pub fn allocate_slot(state_dir: &Path, subnet_cidr: &str) -> Result<VmNetSlot, ProviderError> {
    allocate_slot_bounded(state_dir, subnet_cidr, None, "local")
}

/// Allocates a slot like [`allocate_slot`], returning [`ProviderError::CapacityUnavailable`]
/// once active microVM count reaches `max_instances` so hybrid pools spill onto cloud Spot.
pub fn allocate_slot_bounded(
    state_dir: &Path,
    subnet_cidr: &str,
    max_instances: Option<usize>,
    zone: &str,
) -> Result<VmNetSlot, ProviderError> {
    let (network, mask, prefix_len) = parse_subnet_cidr(subnet_cidr)?;
    let existing = list_metadata(state_dir)?;
    if let Some(limit) = max_instances
        && existing.len() >= limit
    {
        return Err(ProviderError::CapacityUnavailable {
            zone: zone.to_owned(),
        });
    }

    let mut used_ips = BTreeSet::new();
    let mut used_macs = BTreeSet::new();
    let mut used_taps = BTreeSet::new();
    for meta in existing {
        used_ips.insert(meta.guest_ip);
        used_macs.insert(meta.mac_addr);
        used_taps.insert(meta.tap_name);
    }

    let host_count = !mask;
    let gateway_ip = Ipv4Addr::from(network + 1).to_string();
    let netmask = Ipv4Addr::from(mask).to_string();

    for offset in 2..host_count {
        let ip = Ipv4Addr::from(network + offset);
        let [a, b, c, d] = ip.octets();
        if d == 0 || d == 255 || (prefix_len <= 16 && c == 255) {
            continue;
        }
        let guest_ip = ip.to_string();
        let mac_addr = format!("02:c1:{a:02x}:{b:02x}:{c:02x}:{d:02x}");
        let tap_name = format!("oci-{offset:04x}");
        if tap_name.len() > MAX_IFNAMSIZ {
            return Err(ProviderError::InvalidConfig(
                "allocated TAP interface name exceeds Linux IFNAMSIZ".to_owned(),
            ));
        }
        if !used_ips.contains(&guest_ip)
            && !used_macs.contains(&mac_addr)
            && !used_taps.contains(&tap_name)
        {
            return Ok(VmNetSlot {
                slot_index: offset,
                guest_ip,
                gateway_ip,
                netmask,
                mac_addr,
                tap_name,
            });
        }
    }

    Err(ProviderError::CapacityUnavailable {
        zone: "local".to_owned(),
    })
}

fn parse_subnet_cidr(subnet_cidr: &str) -> Result<(u32, u32, u32), ProviderError> {
    let Some((ip_str, prefix_str)) = subnet_cidr.trim().split_once('/') else {
        return Err(ProviderError::InvalidConfig(format!(
            "invalid subnet CIDR `{subnet_cidr}`"
        )));
    };
    let base_ip: Ipv4Addr = ip_str.parse().map_err(|_| {
        ProviderError::InvalidConfig(format!("invalid IPv4 base in subnet CIDR `{subnet_cidr}`"))
    })?;
    let prefix_len: u32 = prefix_str.parse().map_err(|_| {
        ProviderError::InvalidConfig(format!("invalid prefix length in `{subnet_cidr}`"))
    })?;
    if !(8..=30).contains(&prefix_len) {
        return Err(ProviderError::InvalidConfig(format!(
            "subnet CIDR prefix length `{prefix_len}` must be within 8..=30"
        )));
    }
    let mask = u32::MAX.checked_shl(32 - prefix_len).unwrap_or(0);
    let network = u32::from(base_ip) & mask;
    Ok((network, mask, prefix_len))
}

/// Atomically persists `VmMetadata` to `<vm_dir>/meta.json`.
pub fn write_metadata(path: &Path, meta: &VmMetadata) -> Result<(), ProviderError> {
    let encoded = serde_json::to_vec_pretty(meta)
        .map_err(|err| ProviderError::Transient(format!("failed serializing meta.json: {err}")))?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, encoded)
        .map_err(|err| ProviderError::Transient(format!("failed writing meta.json.tmp: {err}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|err| ProviderError::Transient(format!("failed committing meta.json: {err}")))
}

/// Reads `VmMetadata` from `<vm_dir>/meta.json`, returning `Ok(None)` if absent.
pub fn read_metadata(path: &Path) -> Result<Option<VmMetadata>, ProviderError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let parsed: VmMetadata = serde_json::from_slice(&bytes).map_err(|err| {
                ProviderError::Transient(format!("invalid meta.json at {}: {err}", path.display()))
            })?;
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ProviderError::Transient(format!(
            "failed reading meta.json: {err}"
        ))),
    }
}

/// Lists all valid `VmMetadata` records persisted beneath `state_dir`.
pub fn list_metadata(state_dir: &Path) -> Result<Vec<VmMetadata>, ProviderError> {
    let entries = match std::fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(ProviderError::Transient(format!(
                "failed reading Cloud Hypervisor state_dir: {err}"
            )));
        }
    };

    let mut items = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path().join("meta.json");
        if let Ok(Some(meta)) = read_metadata(&meta_path) {
            items.push(meta);
        }
    }
    Ok(items)
}
