//! Native pure-Rust Cloud Hypervisor micro-VM compute provider for `open-ci`.

pub mod api;
mod host;
pub mod vm;

pub use api::{
    ChApiError, ConsoleConfig, CpuAffinity, CpuTopology, CpusConfig, DiskConfig, DiskQueueAffinity,
    MemoryConfig, NetConfig, PayloadConfig, RngConfig, VmConfig, VmInfoResponse, parse_vm_state,
};
pub use vm::{
    MAX_IFNAMSIZ, SEED_MAGIC, SEED_SECTOR_SIZE, VmMetadata, VmNetSlot, allocate_slot,
    allocate_slot_bounded, build_kernel_cmdline, decode_seed_image, encode_seed_image,
    parse_machine_shape,
};

use async_trait::async_trait;
use host::{
    cleanup_failed_launch, clone_rootfs, compute_disjoint_affinities, create_private_dir,
    create_tap, delete_tap, require_file, spawn_ch_process, terminate_matching_pid,
    wait_for_socket,
};
use open_ci_core::ProviderKind;
use open_ci_core::config::{CloudHypervisorProviderConfig, Placement};
use open_ci_provider::{
    CloudDisk, CloudInstance, CloudProvider, CreateReceipt, InstanceSpec, InstanceStatus,
    OperationRef, OperationStatus, ProviderError,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Child;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

const MIB_BYTES: u64 = 1_048_576;
const LIST_INSTANCES_CONCURRENCY: usize = 16;

static HEALTH_PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Controls whether [`CloudHypervisorProvider`] manages local TAP devices and
/// spawns `cloud-hypervisor` processes or talks to an externally provided Unix socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeMode {
    Native,
    ExternalSocket { socket_path: PathBuf },
}

/// [`CloudProvider`] backed by local KVM micro-VMs via Cloud Hypervisor's Unix REST API.
#[derive(Debug)]
pub struct CloudHypervisorProvider {
    config: CloudHypervisorProviderConfig,
    runtime: RuntimeMode,
    alloc_lock: Arc<Mutex<()>>,
}

impl CloudHypervisorProvider {
    pub fn new(config: CloudHypervisorProviderConfig) -> Self {
        Self::with_runtime_mode(config, RuntimeMode::Native)
    }

    pub fn with_runtime_mode(config: CloudHypervisorProviderConfig, runtime: RuntimeMode) -> Self {
        Self {
            config,
            runtime,
            alloc_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Constructs the typed Cloud Hypervisor `PUT /api/v1/vm.create` payload for a micro-VM.
    pub fn build_vm_config(
        &self,
        spec: &InstanceSpec,
        slot: &VmNetSlot,
        rootfs_path: &Path,
        seed_path: &Path,
        console_log_path: &Path,
    ) -> Result<VmConfig, ProviderError> {
        let (vcpus, memory_mib) = parse_machine_shape(
            &spec.placement.machine_type,
            self.config.default_vcpus,
            self.config.default_memory_mib,
        );
        let desired_tpc = spec
            .placement
            .threads_per_core
            .unwrap_or(if self.config.pin_physical_cpus { 1 } else { 2 });
        let threads_per_core = match desired_tpc {
            2 if vcpus > 1 && (vcpus & 1) == 0 => 2,
            _ => 1,
        };
        let cores_per_die = (vcpus / threads_per_core).max(1);
        let memory_bytes = memory_mib
            .checked_mul(MIB_BYTES)
            .ok_or_else(|| ProviderError::InvalidConfig("memory_mib overflow".to_owned()))?;
        let cmdline = build_kernel_cmdline(&self.config.kernel_cmdline, slot, &spec.worker_name);
        let blk_queues = self.config.virtio_blk_queues;
        let (cpu_affinity, disk_queue_affinity) = compute_disjoint_affinities(
            self.config.pin_physical_cpus,
            slot.slot_index,
            vcpus,
            blk_queues,
        );

        Ok(VmConfig {
            cpus: CpusConfig {
                boot_vcpus: vcpus,
                max_vcpus: vcpus,
                topology: CpuTopology {
                    threads_per_core,
                    cores_per_die,
                    dies_per_package: 1,
                    packages: 1,
                },
                affinity: cpu_affinity,
            },
            memory: MemoryConfig {
                size: memory_bytes,
                hugepages: self.config.memory_hugepages,
                prefault: self.config.memory_hugepages,
            },
            payload: PayloadConfig {
                kernel: self.config.kernel_path.clone(),
                cmdline,
            },
            disks: vec![
                DiskConfig {
                    path: rootfs_path.display().to_string(),
                    readonly: false,
                    direct: self.config.host_direct_io,
                    num_queues: blk_queues,
                    queue_size: 1024,
                    queue_affinity: disk_queue_affinity,
                },
                DiskConfig {
                    path: seed_path.display().to_string(),
                    readonly: true,
                    direct: false,
                    num_queues: 1,
                    queue_size: 128,
                    queue_affinity: Vec::new(),
                },
            ],
            net: vec![NetConfig {
                tap: slot.tap_name.clone(),
                mac: slot.mac_addr.clone(),
                num_queues: 2,
                queue_size: 1024,
                offload_tso: true,
                offload_ufo: true,
                offload_csum: true,
            }],
            rng: RngConfig {
                src: "/dev/urandom".to_owned(),
            },
            serial: ConsoleConfig {
                mode: "File".to_owned(),
                file: Some(console_log_path.display().to_string()),
            },
        })
    }

    fn state_dir(&self) -> PathBuf {
        PathBuf::from(&self.config.state_dir)
    }

    async fn reserve_vm_slot(
        &self,
        spec: &InstanceSpec,
    ) -> Result<(PathBuf, VmNetSlot, VmMetadata), ProviderError> {
        let _guard = self.alloc_lock.lock().await;
        let state_dir = self.state_dir();
        let vm_dir = state_dir.join(&spec.worker_name);
        if tokio::fs::try_exists(&vm_dir).await.unwrap_or(false) {
            return Err(ProviderError::Transient(format!(
                "microVM directory already exists for `{}`",
                spec.worker_name
            )));
        }

        let subnet = self.config.subnet_cidr.clone();
        let max_instances = self.config.max_instances;
        let zone = spec.placement.zone.clone();
        let slot = tokio::task::spawn_blocking(move || {
            allocate_slot_bounded(&state_dir, &subnet, max_instances, &zone)
        })
        .await
        .map_err(|_| ProviderError::Transient("slot allocation task failed".to_owned()))??;

        let dir_to_create = vm_dir.clone();
        tokio::task::spawn_blocking(move || create_private_dir(&dir_to_create))
            .await
            .map_err(|_| ProviderError::Transient("dir creation task failed".to_owned()))??;

        let socket_path = match &self.runtime {
            RuntimeMode::Native => vm_dir.join("api.sock"),
            RuntimeMode::ExternalSocket { socket_path } => socket_path.clone(),
        };
        let meta = VmMetadata {
            worker_name: spec.worker_name.clone(),
            pool_id: spec.pool_id.clone(),
            zone: spec.placement.zone.clone(),
            pid: 0,
            socket_path,
            tap_name: slot.tap_name.clone(),
            guest_ip: slot.guest_ip.clone(),
            gateway_ip: slot.gateway_ip.clone(),
            mac_addr: slot.mac_addr.clone(),
            rootfs_path: vm_dir.join("rootfs.img"),
            seed_path: vm_dir.join("seed.raw"),
            expires_at_unix: spec.expires_at_unix,
        };
        let meta_path = vm_dir.join("meta.json");
        let meta_to_save = meta.clone();
        if let Err(err) =
            tokio::task::spawn_blocking(move || vm::write_metadata(&meta_path, &meta_to_save))
                .await
                .map_err(|_| ProviderError::Transient("metadata write task failed".to_owned()))
                .and_then(|res| res)
        {
            let _ = tokio::fs::remove_dir_all(&vm_dir).await;
            return Err(err);
        }

        Ok((vm_dir, slot, meta))
    }

    async fn launch_vm(
        &self,
        spec: &InstanceSpec,
        vm_dir: &Path,
        slot: &VmNetSlot,
        meta: &mut VmMetadata,
        child_out: &mut Option<Child>,
        tap_created: &mut bool,
    ) -> Result<(), ProviderError> {
        let rootfs_src = PathBuf::from(
            spec.placement
                .image
                .as_deref()
                .unwrap_or(&self.config.rootfs_image),
        );
        clone_rootfs(&rootfs_src, &meta.rootfs_path).await?;

        let seed_path = meta.seed_path.clone();
        let user_data = spec.user_data.clone();
        tokio::task::spawn_blocking(move || vm::write_seed_image(&seed_path, &user_data))
            .await
            .map_err(|_| ProviderError::Transient("seed write task failed".to_owned()))??;

        if matches!(self.runtime, RuntimeMode::Native) {
            create_tap(&slot.tap_name, &self.config.bridge_name).await?;
            *tap_created = true;

            let spawned = spawn_ch_process(&self.config.binary, &meta.socket_path)?;
            meta.pid = spawned.id().unwrap_or(0);
            *child_out = Some(spawned);

            let meta_path = vm_dir.join("meta.json");
            let persisted = meta.clone();
            tokio::task::spawn_blocking(move || vm::write_metadata(&meta_path, &persisted))
                .await
                .map_err(|_| {
                    ProviderError::Transient("metadata commit task failed".to_owned())
                })??;

            if let Some(child) = child_out.as_mut() {
                wait_for_socket(&meta.socket_path, child).await?;
            }
        }

        let console_log_path = vm_dir.join("console.log");
        let vm_config = self.build_vm_config(
            spec,
            slot,
            &meta.rootfs_path,
            &meta.seed_path,
            &console_log_path,
        )?;
        api::vm_create(&meta.socket_path, &vm_config).await?;
        api::vm_boot(&meta.socket_path).await?;

        tracing::info!(
            worker = %spec.worker_name,
            guest_ip = %slot.guest_ip,
            tap = %slot.tap_name,
            "booted Cloud Hypervisor microVM"
        );
        Ok(())
    }
}

#[async_trait]
impl CloudProvider for CloudHypervisorProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::CloudHypervisor
    }

    async fn health_check(&self, _placement: &Placement) -> Result<(), ProviderError> {
        require_file("cloud_hypervisor.binary", Path::new(&self.config.binary)).await?;
        require_file(
            "cloud_hypervisor.kernel_path",
            Path::new(&self.config.kernel_path),
        )
        .await?;
        require_file(
            "cloud_hypervisor.rootfs_image",
            Path::new(&self.config.rootfs_image),
        )
        .await?;

        let state_dir = self.state_dir();
        tokio::fs::create_dir_all(&state_dir).await.map_err(|err| {
            ProviderError::InvalidConfig(format!(
                "cannot create state_dir {}: {err}",
                state_dir.display()
            ))
        })?;
        let seq = HEALTH_PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let probe = state_dir.join(format!(".open-ci-healthcheck-{}-{seq}", std::process::id()));
        tokio::fs::write(&probe, b"ok").await.map_err(|err| {
            ProviderError::InvalidConfig(format!(
                "state_dir {} is not writable: {err}",
                state_dir.display()
            ))
        })?;
        let _ = tokio::fs::remove_file(&probe).await;
        Ok(())
    }

    async fn get_instance(
        &self,
        _placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<CloudInstance>, ProviderError> {
        let name = provider_instance_id.unwrap_or(worker_name);
        vm::validate_worker_name(name)?;
        let meta_path = self.state_dir().join(name).join("meta.json");
        let meta = tokio::task::spawn_blocking(move || vm::read_metadata(&meta_path))
            .await
            .map_err(|_| ProviderError::Transient("metadata read task failed".to_owned()))??;
        let Some(meta) = meta else {
            return Ok(None);
        };
        Ok(Some(metadata_to_instance(meta).await?))
    }

    async fn list_instances(
        &self,
        _placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudInstance>, ProviderError> {
        let state_dir = self.state_dir();
        let records = tokio::task::spawn_blocking(move || vm::list_metadata(&state_dir))
            .await
            .map_err(|_| ProviderError::Transient("metadata list task failed".to_owned()))??;
        let sem = Arc::new(Semaphore::new(LIST_INSTANCES_CONCURRENCY));
        let mut tasks = JoinSet::new();
        for meta in records {
            if meta.pool_id != pool_id {
                continue;
            }
            let sem = Arc::clone(&sem);
            tasks.spawn(async move {
                let _permit = sem.acquire_owned().await.ok()?;
                match metadata_to_instance(meta).await {
                    Ok(inst) => Some(inst),
                    Err(err) => {
                        tracing::debug!(error = %err, "skipping microVM after transient vm.info error");
                        None
                    }
                }
            });
        }
        let mut instances = Vec::new();
        while let Some(res) = tasks.join_next().await {
            if let Ok(Some(inst)) = res {
                instances.push(inst);
            }
        }
        Ok(instances)
    }

    async fn list_disks(
        &self,
        _placement: &Placement,
        _pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError> {
        Ok(Vec::new())
    }

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError> {
        vm::validate_worker_name(&spec.worker_name)?;
        let (vm_dir, slot, mut meta) = self.reserve_vm_slot(spec).await?;
        let mut child = None;
        let mut tap_created = false;

        if let Err(err) = self
            .launch_vm(
                spec,
                &vm_dir,
                &slot,
                &mut meta,
                &mut child,
                &mut tap_created,
            )
            .await
        {
            cleanup_failed_launch(&vm_dir, &slot.tap_name, child, tap_created).await;
            return Err(err);
        }

        Ok(CreateReceipt {
            provider_instance_id: Some(spec.worker_name.clone()),
            operation: None,
        })
    }

    async fn poll_operation(
        &self,
        _placement: &Placement,
        _operation: &OperationRef,
    ) -> Result<OperationStatus, ProviderError> {
        Ok(OperationStatus::Done)
    }

    async fn delete_instance(
        &self,
        _placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        let name = provider_instance_id.unwrap_or(worker_name);
        vm::validate_worker_name(name)?;
        let vm_dir = self.state_dir().join(name);
        let meta_path = vm_dir.join("meta.json");
        let meta = tokio::task::spawn_blocking(move || vm::read_metadata(&meta_path))
            .await
            .map_err(|_| ProviderError::Transient("metadata read task failed".to_owned()))??;

        if let Some(meta) = meta {
            if let Err(err) = api::vm_shutdown(&meta.socket_path).await {
                tracing::debug!(
                    worker = %meta.worker_name,
                    error = %err,
                    "vm.shutdown returned error during delete_instance"
                );
            }
            let vmm_stopped = api::vmm_shutdown(&meta.socket_path).await.is_ok();
            if matches!(self.runtime, RuntimeMode::Native) {
                if !vmm_stopped && meta.pid > 0 {
                    terminate_matching_pid(meta.pid, &meta.socket_path).await;
                }
                delete_tap(&meta.tap_name).await;
            }
        }

        if let Err(err) = tokio::fs::remove_dir_all(&vm_dir).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ProviderError::Transient(format!(
                "failed removing microVM directory {}: {err}",
                vm_dir.display()
            )));
        }
        Ok(None)
    }

    async fn delete_disk(
        &self,
        _placement: &Placement,
        _disk_name: &str,
        _provider_disk_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        Ok(None)
    }
}

async fn metadata_to_instance(meta: VmMetadata) -> Result<CloudInstance, ProviderError> {
    let status = match api::vm_info(&meta.socket_path).await {
        Ok(info) => parse_vm_state(&info.state),
        Err(ProviderError::NotFound) => InstanceStatus::Terminated,
        Err(err) => return Err(err),
    };
    Ok(CloudInstance {
        provider_id: meta.worker_name.clone(),
        name: meta.worker_name,
        pool_id: meta.pool_id,
        status,
        zone: meta.zone,
    })
}
