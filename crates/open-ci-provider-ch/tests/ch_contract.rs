//! Unix-socket wire-contract and unit tests for `CloudHypervisorProvider`.

use open_ci_provider_ch::vm::{self, MAX_IFNAMSIZ, VmMetadata};
use open_ci_provider_ch::{
    SEED_MAGIC, SEED_SECTOR_SIZE, allocate_slot, decode_seed_image, encode_seed_image,
    parse_machine_shape,
};
use tempfile::TempDir;

#[cfg(unix)]
use open_ci_core::ProviderKind;
#[cfg(unix)]
use open_ci_core::config::{CloudHypervisorProviderConfig, Placement};
#[cfg(unix)]
use open_ci_provider::{
    CloudProvider, InstanceSpec, InstanceStatus, OperationRef, OperationStatus, ProviderError,
};
#[cfg(unix)]
use open_ci_provider_ch::{CloudHypervisorProvider, RuntimeMode};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::sync::Mutex;

#[cfg(unix)]
fn test_placement(machine_type: &str) -> Placement {
    Placement {
        provider: ProviderKind::CloudHypervisor,
        region: "local".to_owned(),
        zone: "local-1".to_owned(),
        machine_type: machine_type.to_owned(),
        subnet: "open-ci-br0".to_owned(),
        spot: false,
        disk_type: None,
        disk_interface: None,
        nic_type: None,
        image: None,
        security_group: None,
        root_device: None,
        max_spot_price_usd: None,
        core_count: None,
        threads_per_core: None,
        local_ssd_count: None,
        network_bandwidth_tier: None,
        ebs_iops: None,
        ebs_throughput_mbps: None,
        placement_group: None,
    }
}

#[cfg(unix)]
fn test_ch_config(root: &Path) -> Result<CloudHypervisorProviderConfig, std::io::Error> {
    let binary = root.join("cloud-hypervisor");
    let kernel = root.join("vmlinux");
    let rootfs = root.join("runner.raw");
    let state_dir = root.join("microvms");
    std::fs::write(&binary, b"fake-binary")?;
    std::fs::write(&kernel, b"fake-kernel")?;
    std::fs::write(&rootfs, b"fake-rootfs-bytes")?;
    Ok(CloudHypervisorProviderConfig {
        binary: binary.display().to_string(),
        state_dir: state_dir.display().to_string(),
        kernel_path: kernel.display().to_string(),
        rootfs_image: rootfs.display().to_string(),
        kernel_cmdline: "console=ttyS0 root=/dev/vda rw init=/sbin/init quiet".to_owned(),
        bridge_name: "open-ci-br0".to_owned(),
        subnet_cidr: "10.200.0.0/16".to_owned(),
        host_direct_io: false,
        virtio_blk_queues: 4,
        memory_hugepages: true,
        pin_physical_cpus: true,
        default_vcpus: 4,
        default_memory_mib: 16_384,
        max_instances: None,
    })
}

#[cfg(unix)]
#[derive(Default)]
struct MockChState {
    vm_state: String,
    create_payload: Option<Value>,
    requests: Vec<String>,
}

#[cfg(unix)]
async fn read_mock_request(
    stream: &mut tokio::net::UnixStream,
) -> Result<(String, String, Vec<u8>), std::io::Error> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof before headers",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_ascii_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0_usize;
    for line in lines {
        if let Some((name, val)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
            && let Ok(parsed) = val.trim().parse::<usize>()
        {
            content_length = parsed;
        }
    }

    let body_start = header_end + 4;
    let total = body_start + content_length;
    while buf.len() < total {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    let body = buf.get(body_start..total).unwrap_or_default().to_vec();
    Ok((method, path, body))
}

#[cfg(unix)]
fn spawn_mock_ch_server(socket_path: &Path) -> Result<Arc<Mutex<MockChState>>, std::io::Error> {
    let listener = UnixListener::bind(socket_path)?;
    let state = Arc::new(Mutex::new(MockChState {
        vm_state: "Created".to_owned(),
        create_payload: None,
        requests: Vec::new(),
    }));
    let server_state = Arc::clone(&state);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let state_clone = Arc::clone(&server_state);
            tokio::spawn(async move {
                let Ok((method, path, body)) = read_mock_request(&mut stream).await else {
                    return;
                };
                let mut guard = state_clone.lock().await;
                guard.requests.push(format!("{method} {path}"));
                let reply = match (method.as_str(), path.as_str()) {
                    ("PUT", "/api/v1/vm.create") => {
                        guard.create_payload = serde_json::from_slice::<Value>(&body).ok();
                        "Created".clone_into(&mut guard.vm_state);
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned()
                    }
                    ("PUT", "/api/v1/vm.boot") => {
                        "Running".clone_into(&mut guard.vm_state);
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned()
                    }
                    ("GET", "/api/v1/vm.info") => {
                        let json = format!("{{\"state\":\"{}\"}}", guard.vm_state);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
                            json.len()
                        )
                    }
                    ("PUT", "/api/v1/vm.shutdown" | "/api/v1/vmm.shutdown") => {
                        "Shutdown".clone_into(&mut guard.vm_state);
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned()
                    }
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_owned(),
                };
                drop(guard);
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });

    Ok(state)
}

#[cfg(unix)]
fn assert_vm_create_payload(payload: &Value, cfg: &CloudHypervisorProviderConfig) {
    assert_eq!(payload["cpus"]["boot_vcpus"], 4);
    assert_eq!(payload["cpus"]["max_vcpus"], 4);
    assert_eq!(payload["cpus"]["topology"]["threads_per_core"], 1);
    assert_eq!(payload["cpus"]["topology"]["cores_per_die"], 4);
    assert_eq!(payload["cpus"]["topology"]["dies_per_package"], 1);
    assert_eq!(payload["cpus"]["topology"]["packages"], 1);
    assert_eq!(payload["memory"]["size"], 16_384_u64 * 1_048_576);
    assert_eq!(payload["memory"]["hugepages"], true);
    assert_eq!(payload["memory"]["prefault"], true);
    assert_eq!(payload["payload"]["kernel"], cfg.kernel_path);
    let cmdline = payload["payload"]["cmdline"].as_str().unwrap_or_default();
    assert!(
        cmdline.contains("ip=10.200.0.2::10.200.0.1:255.255.0.0:open-ci-ch-1:eth0:off"),
        "unexpected cmdline: {cmdline}"
    );
    assert_eq!(payload["disks"][0]["readonly"], false);
    assert_eq!(payload["disks"][0]["direct"], false);
    assert_eq!(payload["disks"][0]["num_queues"], 4);
    assert_eq!(payload["disks"][0]["queue_size"], 1024);
    assert_eq!(payload["disks"][1]["readonly"], true);
    assert!(
        payload["disks"][1]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("seed.raw"))
    );
    let tap = payload["net"][0]["tap"].as_str().unwrap_or_default();
    assert!(tap.starts_with("oci-"));
    assert!(tap.len() <= MAX_IFNAMSIZ);
    assert_eq!(payload["net"][0]["num_queues"], 2);
    assert_eq!(payload["net"][0]["queue_size"], 1024);
    assert_eq!(payload["net"][0]["offload_tso"], true);
    assert_eq!(payload["net"][0]["offload_ufo"], true);
    assert_eq!(payload["net"][0]["offload_csum"], true);
}

#[cfg(unix)]
fn assert_seed_on_disk(vm_dir: &Path, expected_user_data: &str) {
    let seed_bytes = std::fs::read(vm_dir.join("seed.raw"));
    assert!(seed_bytes.is_ok());
    if let Ok(raw) = seed_bytes {
        assert_eq!(raw.len() & (SEED_SECTOR_SIZE - 1), 0);
        assert_eq!(&raw[..8], SEED_MAGIC);
        assert_eq!(
            decode_seed_image(&raw).ok().as_deref(),
            Some(expected_user_data)
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_wire_contract_create_boot_info_shutdown() {
    let dir_res = TempDir::new();
    assert!(dir_res.is_ok());
    let Ok(dir) = dir_res else { return };

    let cfg_res = test_ch_config(dir.path());
    assert!(cfg_res.is_ok());
    let Ok(cfg) = cfg_res else { return };

    let socket_path = dir.path().join("mock-ch.sock");
    let server_res = spawn_mock_ch_server(&socket_path);
    assert!(server_res.is_ok());
    let Ok(server_state) = server_res else { return };

    let provider = CloudHypervisorProvider::with_runtime_mode(
        cfg.clone(),
        RuntimeMode::ExternalSocket { socket_path },
    );
    assert_eq!(provider.kind(), ProviderKind::CloudHypervisor);

    let placement = test_placement("ch-4c-16g");
    assert!(provider.health_check(&placement).await.is_ok());

    let user_data = "{\"bootstrap_url\":\"https://ci.example.com/v1/workers/w1/bootstrap\"}";
    let receipt = provider
        .create_instance(&InstanceSpec {
            worker_name: "open-ci-ch-1".to_owned(),
            pool_id: "fast".to_owned(),
            request_id: "req-ch-1".to_owned(),
            placement: placement.clone(),
            disk_gb: 50,
            lifetime_seconds: 1800,
            expires_at_unix: 2000,
            user_data: user_data.to_owned(),
            tenant_id: None,
        })
        .await;
    assert!(matches!(
        receipt,
        Ok(ref rec)
            if rec.provider_instance_id.as_deref() == Some("open-ci-ch-1")
                && rec.operation.is_none()
    ));

    let vm_dir = PathBuf::from(&cfg.state_dir).join("open-ci-ch-1");
    assert_seed_on_disk(&vm_dir, user_data);

    {
        let guard = server_state.lock().await;
        assert!(guard.requests.iter().any(|r| r == "PUT /api/v1/vm.create"));
        assert!(guard.requests.iter().any(|r| r == "PUT /api/v1/vm.boot"));
        assert!(guard.create_payload.is_some());
        if let Some(payload) = &guard.create_payload {
            assert_vm_create_payload(payload, &cfg);
        }
    }

    let instance = provider
        .get_instance(&placement, "open-ci-ch-1", None)
        .await;
    assert!(matches!(
        instance,
        Ok(Some(ref inst))
            if inst.status == InstanceStatus::Running
                && inst.pool_id == "fast"
                && inst.name == "open-ci-ch-1"
    ));

    let listed = provider.list_instances(&placement, "fast").await;
    assert!(matches!(
        listed,
        Ok(ref items) if items.len() == 1 && items[0].status == InstanceStatus::Running
    ));

    assert_eq!(
        provider
            .poll_operation(&placement, &OperationRef("noop".to_owned()))
            .await,
        Ok(OperationStatus::Done)
    );
    assert_eq!(
        provider.list_disks(&placement, "fast").await,
        Ok(Vec::new())
    );
    assert_eq!(
        provider.delete_disk(&placement, "disk-1", None).await,
        Ok(None)
    );

    assert!(
        provider
            .delete_instance(&placement, "open-ci-ch-1", None)
            .await
            .is_ok()
    );
    assert!(!vm_dir.exists());

    let guard = server_state.lock().await;
    assert!(guard.requests.iter().any(|r| r == "GET /api/v1/vm.info"));
    assert!(
        guard
            .requests
            .iter()
            .any(|r| r == "PUT /api/v1/vm.shutdown")
    );
    assert!(
        guard
            .requests
            .iter()
            .any(|r| r == "PUT /api/v1/vmm.shutdown")
    );
}

#[test]
fn parse_machine_shape_handles_valid_and_fallback_types() {
    assert_eq!(parse_machine_shape("ch-4c-16g", 2, 4096), (4, 16_384));
    assert_eq!(parse_machine_shape("ch-8c-32g", 4, 16_384), (8, 32_768));
    assert_eq!(parse_machine_shape("4c-16g", 2, 4096), (4, 16_384));
    assert_eq!(
        parse_machine_shape("ch-16c-65536m", 4, 16_384),
        (16, 65_536)
    );
    assert_eq!(
        parse_machine_shape("custom-unknown", 4, 16_384),
        (4, 16_384)
    );
    assert_eq!(parse_machine_shape("ch-0c-0g", 4, 16_384), (4, 16_384));
}

#[test]
fn seed_image_header_padding_and_roundtrip() {
    let cases = [
        String::new(),
        "#!/bin/sh\necho hi\n".to_owned(),
        "x".repeat(496),
        "y".repeat(497),
        "z".repeat(2048),
    ];
    for payload in &cases {
        let encoded_res = encode_seed_image(payload);
        assert!(encoded_res.is_ok());
        if let Ok(encoded) = encoded_res {
            assert_eq!(encoded.len() & (SEED_SECTOR_SIZE - 1), 0);
            assert!(encoded.len() >= SEED_SECTOR_SIZE);
            assert_eq!(&encoded[..8], SEED_MAGIC);
            let decoded = decode_seed_image(&encoded);
            assert_eq!(decoded.ok().as_deref(), Some(payload.as_str()));
        }
    }
}

#[test]
fn slot_allocator_is_deterministic_and_collision_free() {
    let dir_res = TempDir::new();
    assert!(dir_res.is_ok());
    let Ok(dir) = dir_res else { return };

    let first_res = allocate_slot(dir.path(), "10.200.0.0/16");
    assert!(first_res.is_ok());
    let Ok(first) = first_res else { return };
    assert_eq!(first.gateway_ip, "10.200.0.1");
    assert_eq!(first.guest_ip, "10.200.0.2");
    assert_eq!(first.netmask, "255.255.0.0");
    assert!(first.tap_name.starts_with("oci-"));
    assert!(first.tap_name.len() <= MAX_IFNAMSIZ);

    let vm1_dir = dir.path().join("worker-1");
    assert!(std::fs::create_dir_all(&vm1_dir).is_ok());
    let meta1 = VmMetadata {
        worker_name: "worker-1".to_owned(),
        pool_id: "fast".to_owned(),
        zone: "local-1".to_owned(),
        pid: 1001,
        socket_path: vm1_dir.join("api.sock"),
        tap_name: first.tap_name.clone(),
        guest_ip: first.guest_ip.clone(),
        gateway_ip: first.gateway_ip.clone(),
        mac_addr: first.mac_addr.clone(),
        rootfs_path: vm1_dir.join("rootfs.img"),
        seed_path: vm1_dir.join("seed.raw"),
        expires_at_unix: 2000,
    };
    assert!(vm::write_metadata(&vm1_dir.join("meta.json"), &meta1).is_ok());

    let second_res = allocate_slot(dir.path(), "10.200.0.0/16");
    assert!(second_res.is_ok());
    let Ok(second) = second_res else { return };
    assert_eq!(second.gateway_ip, "10.200.0.1");
    assert_eq!(second.guest_ip, "10.200.0.3");
    assert_ne!(second.guest_ip, first.guest_ip);
    assert_ne!(second.mac_addr, first.mac_addr);
    assert_ne!(second.tap_name, first.tap_name);
    assert!(second.tap_name.starts_with("oci-"));
    assert!(second.tap_name.len() <= MAX_IFNAMSIZ);
}

#[cfg(unix)]
#[tokio::test]
async fn max_instances_cap_returns_capacity_unavailable_for_hybrid_spillover() {
    let dir_res = TempDir::new();
    assert!(dir_res.is_ok());
    let Ok(dir) = dir_res else { return };

    let cfg_res = test_ch_config(dir.path());
    assert!(cfg_res.is_ok());
    let Ok(mut cfg) = cfg_res else { return };
    cfg.max_instances = Some(1);

    let socket_path = dir.path().join("mock-ch-cap.sock");
    let server_res = spawn_mock_ch_server(&socket_path);
    assert!(server_res.is_ok());

    let provider = CloudHypervisorProvider::with_runtime_mode(
        cfg.clone(),
        RuntimeMode::ExternalSocket { socket_path },
    );
    let placement = test_placement("ch-4c-16g");

    let mk_spec = |name: &str| InstanceSpec {
        worker_name: name.to_owned(),
        pool_id: "hybrid".to_owned(),
        tenant_id: None,
        request_id: format!("req-{name}"),
        placement: placement.clone(),
        disk_gb: 64,
        lifetime_seconds: 1800,
        expires_at_unix: 2000,
        user_data: "{}".to_owned(),
    };

    assert!(
        provider
            .create_instance(&mk_spec("open-ci-ch-a"))
            .await
            .is_ok()
    );

    let overflow = provider.create_instance(&mk_spec("open-ci-ch-b")).await;
    assert!(matches!(
        overflow,
        Err(ProviderError::CapacityUnavailable { ref zone }) if zone == "local-1"
    ));
    assert!(!PathBuf::from(&cfg.state_dir).join("open-ci-ch-b").exists());

    assert!(
        provider
            .delete_instance(&placement, "open-ci-ch-a", None)
            .await
            .is_ok()
    );
    assert!(
        provider
            .create_instance(&mk_spec("open-ci-ch-b"))
            .await
            .is_ok()
    );
}
