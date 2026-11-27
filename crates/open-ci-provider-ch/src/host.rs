//! Linux host process, TAP interface, `CoW` rootfs cloning, and physical CPU pinning helpers.

use crate::api::{CpuAffinity, DiskQueueAffinity};
use open_ci_provider::ProviderError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

pub(crate) fn create_private_dir(path: &Path) -> Result<(), ProviderError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|err| {
        ProviderError::Transient(format!("failed creating microVM directory: {err}"))
    })
}

pub(crate) async fn require_file(field: &str, path: &Path) -> Result<(), ProviderError> {
    let meta = tokio::fs::metadata(path).await.map_err(|err| {
        ProviderError::InvalidConfig(format!("{field} `{}` missing: {err}", path.display()))
    })?;
    if !meta.is_file() {
        return Err(ProviderError::InvalidConfig(format!(
            "{field} `{}` is not a regular file",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) async fn clone_rootfs(src: &Path, dst: &Path) -> Result<(), ProviderError> {
    let reflink_ok = Command::new("cp")
        .arg("--reflink=auto")
        .arg(src)
        .arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if reflink_ok {
        return Ok(());
    }
    tokio::fs::copy(src, dst)
        .await
        .map(|_| ())
        .map_err(|err| ProviderError::Transient(format!("failed copying rootfs image: {err}")))
}

async fn run_ip(args: &[&str]) -> Result<(), ProviderError> {
    let status = Command::new("ip")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|err| ProviderError::Transient(format!("failed invoking ip command: {err}")))?;
    if !status.success() {
        return Err(ProviderError::Transient(format!(
            "ip {args:?} exited with {status}"
        )));
    }
    Ok(())
}

fn discover_physical_cores() -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::collections::BTreeSet;
        let mut primary = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if !name_str.strip_prefix("cpu").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                }) {
                    continue;
                }
                let siblings_path = entry.path().join("topology/thread_siblings_list");
                if let Ok(raw) = std::fs::read_to_string(siblings_path)
                    && let Some(first) = raw
                        .trim()
                        .split([',', '-'])
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                {
                    primary.insert(first);
                }
            }
        }
        if !primary.is_empty() {
            return primary.into_iter().collect();
        }
    }
    let count = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    (0..count).collect()
}

pub(crate) fn compute_disjoint_affinities(
    pin_physical_cpus: bool,
    slot_index: u32,
    vcpus: u32,
    blk_queues: u32,
) -> (Vec<CpuAffinity>, Vec<DiskQueueAffinity>) {
    if !pin_physical_cpus {
        return (Vec::new(), Vec::new());
    }
    let cores = discover_physical_cores();
    let stride = match usize::try_from(vcpus.saturating_add(blk_queues)) {
        Ok(n) if n > 0 => n,
        _ => return (Vec::new(), Vec::new()),
    };
    let vm_idx = usize::try_from(slot_index.saturating_sub(2)).unwrap_or(usize::MAX);
    let Some(start) = vm_idx.checked_mul(stride) else {
        return (Vec::new(), Vec::new());
    };
    let Some(end) = start.checked_add(stride) else {
        return (Vec::new(), Vec::new());
    };
    if end > cores.len() {
        return (Vec::new(), Vec::new());
    }
    let vcpu_count = usize::try_from(vcpus).unwrap_or(0);
    let cpu_affinity = (0..vcpus)
        .filter_map(|idx| {
            let core = *cores.get(start + usize::try_from(idx).ok()?)?;
            Some(CpuAffinity {
                vcpu: idx,
                host_cpus: vec![core],
            })
        })
        .collect();
    let queue_affinity = (0..blk_queues)
        .filter_map(|idx| {
            let core = *cores.get(start + vcpu_count + usize::try_from(idx).ok()?)?;
            Some(DiskQueueAffinity {
                queue_index: idx,
                host_cpus: vec![core],
            })
        })
        .collect();
    (cpu_affinity, queue_affinity)
}

pub(crate) async fn create_tap(tap_name: &str, bridge_name: &str) -> Result<(), ProviderError> {
    run_ip(&["tuntap", "add", "dev", tap_name, "mode", "tap"]).await?;
    if let Err(err) = run_ip(&["link", "set", tap_name, "master", bridge_name]).await {
        delete_tap(tap_name).await;
        return Err(err);
    }
    if let Err(err) = run_ip(&["link", "set", tap_name, "up"]).await {
        delete_tap(tap_name).await;
        return Err(err);
    }
    Ok(())
}

pub(crate) async fn delete_tap(tap_name: &str) {
    let _ = run_ip(&["link", "del", tap_name]).await;
}

pub(crate) fn spawn_ch_process(binary: &str, socket_path: &Path) -> Result<Child, ProviderError> {
    Command::new(binary)
        .arg("--api-socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| ProviderError::Transient(format!("failed spawning cloud-hypervisor: {err}")))
}

pub(crate) async fn wait_for_socket(
    socket_path: &Path,
    child: &mut Child,
) -> Result<(), ProviderError> {
    for _ in 0..200 {
        if tokio::fs::try_exists(socket_path).await.unwrap_or(false) {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(ProviderError::Transient(format!(
                "cloud-hypervisor exited before creating API socket ({status})"
            )));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(ProviderError::Timeout)
}

fn pid_matches_socket(pid: u32, socket_path: &Path) -> bool {
    let Some(sock) = socket_path.to_str() else {
        return false;
    };
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline.split(|&b| b == 0).any(|arg| arg == sock.as_bytes())
}

pub(crate) async fn terminate_matching_pid(pid: u32, socket_path: &Path) {
    let sock = socket_path.to_path_buf();
    let matches = tokio::task::spawn_blocking(move || pid_matches_socket(pid, &sock))
        .await
        .unwrap_or(false);
    if !matches {
        return;
    }
    let pid_str = pid.to_string();
    let _ = Command::new("kill")
        .args(["-TERM", &pid_str])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let sock = socket_path.to_path_buf();
    let still_matches = tokio::task::spawn_blocking(move || pid_matches_socket(pid, &sock))
        .await
        .unwrap_or(false);
    if still_matches {
        let _ = Command::new("kill")
            .args(["-KILL", &pid_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

pub(crate) async fn cleanup_failed_launch(
    vm_dir: &Path,
    tap_name: &str,
    child: Option<Child>,
    tap_created: bool,
) {
    if let Some(mut process) = child {
        let _ = process.kill().await;
        let _ = process.wait().await;
    }
    if tap_created {
        delete_tap(tap_name).await;
    }
    let _ = tokio::fs::remove_dir_all(vm_dir).await;
}
