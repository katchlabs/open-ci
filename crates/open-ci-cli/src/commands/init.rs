//! `open-ci init` — generates a validated `open-ci.toml` from opinionated presets.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use open_ci_core::OpenCiConfig;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as IoWrite};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum InitPreset {
    /// GCP Spot VMs with local `NVMe` SSDs (c4d x64 + optional c4a arm64).
    GcpSpot,
    /// AWS Spot VMs with local `NVMe` SSDs (c7ad x64 + optional c8gd arm64).
    AwsSpot,
    /// AWS + GCP multi-cloud Spot failover in one pool.
    MultiCloud,
    /// Local bare-metal / nested-KVM Cloud Hypervisor microVMs.
    BareMetal,
    /// Cloud Hypervisor bare-metal baseline with automatic GCP/AWS Spot burst spillover.
    Hybrid,
}

#[derive(Clone, Debug, Args)]
pub struct InitArgs {
    /// Preset architecture to generate.
    #[arg(long, value_enum, default_value = "gcp-spot")]
    pub preset: InitPreset,
    /// Single GitHub repository (`owner/repo`). Mutually exclusive with `--org`.
    #[arg(long)]
    pub repo: Option<String>,
    /// GitHub organization name for org-wide runner pools. Mutually exclusive with `--repo`.
    #[arg(long)]
    pub org: Option<String>,
    /// Public HTTPS URL where GitHub webhooks and worker bootstraps reach `open-ci`.
    #[arg(long, default_value = "https://ci.example.com")]
    pub public_url: String,
    /// GCP project ID (used by `gcp-spot`, `multi-cloud`, and `hybrid`).
    #[arg(long, default_value = "your-gcp-project-id")]
    pub gcp_project: String,
    /// 12-digit AWS account ID (used by `aws-spot` and `multi-cloud`).
    #[arg(long, default_value = "123456789012")]
    pub aws_account: String,
    /// Primary cloud region override.
    #[arg(long)]
    pub region: Option<String>,
    /// Comma-separated zone failover order (GCP presets default to `<region>-a,<region>-b,<region>-c`;
    /// `aws-spot` uses the first zone). Required for GCP regions without an `-a` zone (e.g. `us-east1`).
    #[arg(long = "zone", value_delimiter = ',')]
    pub zones: Vec<String>,
    /// Include an ARM64 runner pool (`open-ci-linux-arm64`) alongside x64.
    #[arg(long)]
    pub arm64: bool,
    /// Enable the built-in transparent GitHub Actions v2 cache (`[cache]`).
    #[arg(long)]
    pub with_cache: bool,
    /// Private runner-reachable cache URL when `--with-cache` is enabled.
    #[arg(long)]
    pub cache_url: Option<String>,
    /// Maximum concurrent instances per pool.
    #[arg(long, default_value_t = 8)]
    pub max_instances: usize,
    /// Warm standby runners to keep ready for sub-second queue time.
    #[arg(long, default_value_t = 0)]
    pub min_idle: usize,
    /// Maximum local microVMs before spilling to Spot in `hybrid` mode.
    #[arg(long, default_value_t = 4)]
    pub local_max_instances: usize,
    /// Output path (`-` prints to stdout).
    #[arg(long, short = 'o', default_value = "open-ci.toml")]
    pub output: PathBuf,
    /// Overwrite an existing output file.
    #[arg(long)]
    pub force: bool,
}

pub fn run_init(args: &InitArgs) -> Result<()> {
    let resolved = if args.repo.is_none()
        && args.org.is_none()
        && args.output.as_os_str() != "-"
        && std::io::stdin().is_terminal()
    {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut stderr = std::io::stderr();
        prompt_args_from_reader(args, &mut reader, &mut stderr)?
    } else {
        args.clone()
    };

    let rendered = render_init_config(&resolved)?;
    let to_stdout = resolved.output.as_os_str() == "-";
    if to_stdout {
        print!("{rendered}");
        return Ok(());
    }

    if resolved.output.exists() && !resolved.force {
        bail!(
            "{} already exists (pass --force to overwrite or --output - for stdout)",
            resolved.output.display()
        );
    }
    if let Some(parent) = resolved.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    std::fs::write(&resolved.output, &rendered)
        .with_context(|| format!("failed writing {}", resolved.output.display()))?;

    let serve_flag = if resolved.with_cache {
        " --with-cache"
    } else {
        ""
    };
    println!("Wrote validated config to {}", resolved.output.display());
    println!();
    println!("Next steps:");
    println!(
        "  1. Replace any AWS placeholder IDs in {} (GCP uses public {PUBLIC_GCP_IMAGE_PROJECT} images)",
        resolved.output.display()
    );
    println!(
        "  2. Run preflight checks:  open-ci doctor --config {}{serve_flag}",
        resolved.output.display()
    );
    println!(
        "  3. Start open-ci:         open-ci serve --config {}{serve_flag}",
        resolved.output.display()
    );
    println!("  4. Use in GitHub Actions: runs-on: open-ci-linux-x64");
    Ok(())
}

fn prompt_line<R: BufRead, W: IoWrite>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: &str,
) -> Result<String> {
    write!(writer, "{prompt} [{default}]: ")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

fn parse_preset_name(input: &str) -> Result<InitPreset> {
    match input.trim().to_ascii_lowercase().as_str() {
        "gcp-spot" | "gcp" => Ok(InitPreset::GcpSpot),
        "aws-spot" | "aws" => Ok(InitPreset::AwsSpot),
        "multi-cloud" | "multicloud" => Ok(InitPreset::MultiCloud),
        "bare-metal" | "baremetal" | "ch" => Ok(InitPreset::BareMetal),
        "hybrid" => Ok(InitPreset::Hybrid),
        other => bail!(
            "unknown preset `{other}` (choose gcp-spot, aws-spot, multi-cloud, bare-metal, or hybrid)"
        ),
    }
}

/// Interactive prompt helper for bare `open-ci init` on a terminal.
pub fn prompt_args_from_reader<R: BufRead, W: IoWrite>(
    base: &InitArgs,
    reader: &mut R,
    writer: &mut W,
) -> Result<InitArgs> {
    let mut args = base.clone();
    writeln!(
        writer,
        "open-ci interactive setup (press Enter for defaults)"
    )?;

    let preset_raw = prompt_line(
        reader,
        writer,
        "Preset (gcp-spot / aws-spot / multi-cloud / bare-metal / hybrid)",
        "gcp-spot",
    )?;
    args.preset = parse_preset_name(&preset_raw)?;

    let scope = prompt_line(
        reader,
        writer,
        "GitHub repo (`owner/repo`) or org (`owner`)",
        "example-org/example-repo",
    )?;
    if scope.contains('/') {
        args.repo = Some(scope);
        args.org = None;
    } else {
        args.repo = None;
        args.org = Some(scope);
    }

    args.public_url = prompt_line(
        reader,
        writer,
        "Controller public HTTPS URL",
        &args.public_url,
    )?;

    if matches!(
        args.preset,
        InitPreset::GcpSpot | InitPreset::MultiCloud | InitPreset::Hybrid
    ) {
        args.gcp_project = prompt_line(reader, writer, "GCP project ID", &args.gcp_project)?;
    }
    if matches!(args.preset, InitPreset::AwsSpot | InitPreset::MultiCloud) {
        args.aws_account =
            prompt_line(reader, writer, "12-digit AWS account ID", &args.aws_account)?;
    }

    let cache_ans = prompt_line(
        reader,
        writer,
        "Enable built-in transparent Actions v2 cache? (y/n)",
        if args.with_cache { "y" } else { "n" },
    )?;
    args.with_cache = matches!(cache_ans.to_ascii_lowercase().as_str(), "y" | "yes");

    if args.preset != InitPreset::BareMetal {
        let arm_ans = prompt_line(
            reader,
            writer,
            "Include ARM64 runner pool? (y/n)",
            if args.arm64 { "y" } else { "n" },
        )?;
        args.arm64 = matches!(arm_ans.to_ascii_lowercase().as_str(), "y" | "yes");
    }

    Ok(args)
}

/// Renders and validates a complete `open-ci.toml` for the requested preset.
pub fn render_init_config(args: &InitArgs) -> Result<String> {
    if args.repo.is_some() && args.org.is_some() {
        bail!("pass either `--repo owner/name` or `--org name`, not both");
    }
    if args.max_instances == 0 {
        bail!("`--max-instances` must be >= 1");
    }
    if args.min_idle > args.max_instances {
        bail!("`--min-idle` cannot exceed `--max-instances`");
    }
    if args.local_max_instances == 0 {
        bail!("`--local-max-instances` must be >= 1");
    }

    let mut out = String::with_capacity(2048);
    writeln!(
        out,
        "# Generated by `open-ci init --preset {:?}`",
        args.preset
    )?;
    writeln!(
        out,
        "# GCP uses public pre-baked images and the infra/terraform subnet; replace AWS placeholder IDs before live use."
    )?;
    writeln!(out)?;

    write_scope(&mut out, args)?;
    write_server_and_github(&mut out, args)?;
    write_providers(&mut out, args)?;
    if args.with_cache {
        write_cache(&mut out, args)?;
    }
    write_admission(&mut out, args)?;
    write_pools(&mut out, args)?;

    // Invariant: `open-ci init` never emits a config that fails validation.
    OpenCiConfig::parse_toml(&out)
        .map_err(|err| anyhow!("generated config failed validation: {err}"))?;
    Ok(out)
}

fn write_scope(out: &mut String, args: &InitArgs) -> Result<()> {
    match (&args.repo, &args.org) {
        (Some(repo), None) => writeln!(out, "repository = \"{repo}\"")?,
        (None, Some(org)) => {
            writeln!(out, "organization = \"{org}\"")?;
            writeln!(out, "allowed_repositories = [\"{org}/*\"]")?;
        }
        (None, None) => writeln!(out, "repository = \"example-org/example-repo\"")?,
        (Some(_), Some(_)) => bail!("pass either `--repo owner/name` or `--org name`, not both"),
    }
    writeln!(out)?;
    Ok(())
}

fn write_server_and_github(out: &mut String, args: &InitArgs) -> Result<()> {
    let tick = match args.preset {
        InitPreset::BareMetal | InitPreset::Hybrid => 1,
        InitPreset::GcpSpot | InitPreset::AwsSpot | InitPreset::MultiCloud => 2,
    };
    writeln!(out, "[server]")?;
    writeln!(out, "listen = \"127.0.0.1:8080\"")?;
    writeln!(out, "public_url = \"{}\"", args.public_url)?;
    writeln!(out, "database_path = \"/var/lib/open-ci/open-ci.db\"")?;
    writeln!(out, "tick_seconds = {tick}")?;
    writeln!(out, "request_timeout_seconds = 20")?;
    writeln!(out)?;
    writeln!(out, "[github]")?;
    writeln!(out, "api_url = \"https://api.github.com\"")?;
    writeln!(
        out,
        "oidc_issuer = \"https://token.actions.githubusercontent.com\""
    )?;
    writeln!(out)?;
    Ok(())
}

fn write_providers(out: &mut String, args: &InitArgs) -> Result<()> {
    if matches!(
        args.preset,
        InitPreset::GcpSpot | InitPreset::MultiCloud | InitPreset::Hybrid
    ) {
        writeln!(out, "[gcp]")?;
        writeln!(out, "project = \"{}\"", args.gcp_project)?;
        writeln!(
            out,
            "compute_url = \"https://compute.googleapis.com/compute/v1\""
        )?;
        writeln!(out, "image = \"{}\"", public_gcp_image(PoolArch::X64))?;
        writeln!(
            out,
            "# Direct egress via ephemeral external IPv4 (no Cloud NAT fees or caps); the"
        )?;
        writeln!(
            out,
            "# open-ci-runner tag's deny-all ingress firewall keeps zero inbound ports open."
        )?;
        writeln!(out, "external_ipv4 = true")?;
        writeln!(out, "network_tier = \"PREMIUM\"")?;
        writeln!(out, "network_tags = [\"open-ci-runner\"]")?;
        writeln!(out)?;
    }
    if matches!(args.preset, InitPreset::AwsSpot | InitPreset::MultiCloud) {
        writeln!(out, "[aws]")?;
        writeln!(out, "account_id = \"{}\"", args.aws_account)?;
        writeln!(out)?;
    }
    if matches!(args.preset, InitPreset::BareMetal | InitPreset::Hybrid) {
        writeln!(out, "[cloud_hypervisor]")?;
        writeln!(out, "binary = \"/usr/local/bin/cloud-hypervisor\"")?;
        writeln!(out, "state_dir = \"/var/lib/open-ci/microvms\"")?;
        writeln!(out, "kernel_path = \"/var/lib/open-ci/images/vmlinux\"")?;
        writeln!(
            out,
            "rootfs_image = \"/var/lib/open-ci/images/ubuntu-24.04-rootfs.ext4\""
        )?;
        writeln!(out, "bridge_name = \"open-ci-br0\"")?;
        writeln!(out, "subnet_cidr = \"10.200.0.0/16\"")?;
        writeln!(out, "virtio_blk_queues = 4")?;
        writeln!(out, "memory_hugepages = false")?;
        writeln!(out, "pin_physical_cpus = false")?;
        writeln!(out, "default_vcpus = 4")?;
        writeln!(out, "default_memory_mib = 16384")?;
        if args.preset == InitPreset::Hybrid {
            writeln!(out, "max_instances = {}", args.local_max_instances)?;
        }
        writeln!(out)?;
    }
    Ok(())
}

fn write_cache(out: &mut String, args: &InitArgs) -> Result<()> {
    let default_url = match args.preset {
        InitPreset::BareMetal | InitPreset::Hybrid => "http://10.200.0.1:8090",
        InitPreset::GcpSpot | InitPreset::AwsSpot | InitPreset::MultiCloud => {
            "http://10.0.0.5:8090"
        }
    };
    let listen = match args.preset {
        InitPreset::BareMetal => "10.200.0.1:8090",
        _ => "0.0.0.0:8090",
    };
    let public_url = args.cache_url.as_deref().unwrap_or(default_url);
    writeln!(out, "[cache]")?;
    writeln!(out, "listen = \"{listen}\"")?;
    writeln!(out, "public_url = \"{public_url}\"")?;
    writeln!(out, "hot_cache_bytes = 2147483648")?;
    writeln!(out, "index_path = \"/var/lib/open-ci-cache/index.db\"")?;
    writeln!(out)?;
    writeln!(out, "[cache.auth]")?;
    writeln!(out, "repositories = \"any\"")?;
    writeln!(out)?;
    writeln!(out, "[cache.storage]")?;
    writeln!(out, "type = \"local\"")?;
    writeln!(out, "path = \"/var/lib/open-ci-cache/blobs\"")?;
    writeln!(out)?;
    Ok(())
}

fn write_admission(out: &mut String, args: &InitArgs) -> Result<()> {
    let pool_count: usize = if args.arm64 && args.preset != InitPreset::BareMetal {
        2
    } else {
        1
    };
    let disk_per_inst: usize = match args.preset {
        InitPreset::BareMetal | InitPreset::Hybrid => 64,
        _ => 40,
    };
    let max_inst = args.max_instances.max(1);
    let total_instances = max_inst
        .saturating_mul(pool_count)
        .saturating_mul(2)
        .saturating_add(4);
    let total_vcpu = total_instances.saturating_mul(4);
    let total_disk = total_instances.saturating_mul(disk_per_inst);

    writeln!(out, "[admission]")?;
    writeln!(out, "mode = \"webhook\"")?;
    writeln!(out, "max_run_seconds = 3600")?;
    writeln!(out, "fallback_on_rerun = true")?;
    writeln!(out)?;
    writeln!(out, "[admission.capacity]")?;
    writeln!(out, "vcpu = {total_vcpu}")?;
    writeln!(out, "instances = {total_instances}")?;
    writeln!(out, "disk_gb = {total_disk}")?;
    writeln!(out)?;
    writeln!(out, "[admission.background]")?;
    writeln!(out, "vcpu = 2")?;
    writeln!(out, "instances = 1")?;
    writeln!(out, "disk_gb = 30")?;
    writeln!(out)?;
    writeln!(out, "[admission.guardrails]")?;
    writeln!(out, "max_concurrent_runs = 8")?;
    writeln!(out)?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolArch {
    X64,
    Arm64,
}

impl PoolArch {
    const fn name(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }

    const fn fallback_label(self) -> &'static str {
        match self {
            Self::X64 => "ubuntu-24.04",
            Self::Arm64 => "ubuntu-24.04-arm",
        }
    }

    const fn gcp_machine_type(self) -> &'static str {
        match self {
            Self::X64 => "c4d-standard-8-lssd",
            Self::Arm64 => "c4a-standard-4-lssd",
        }
    }

    const fn gcp_threads_per_core(self) -> Option<u32> {
        match self {
            Self::X64 => Some(1),
            Self::Arm64 => None,
        }
    }

    const fn aws_machine_type(self) -> &'static str {
        match self {
            Self::X64 => "c7ad.xlarge",
            Self::Arm64 => "c8gd.xlarge",
        }
    }

    const fn aws_image(self) -> &'static str {
        match self {
            Self::X64 => "ami-0123456789abcdef0",
            Self::Arm64 => "ami-0fedcba9876543210",
        }
    }
}

/// Project hosting the pre-baked runner images, readable by `allAuthenticatedUsers`.
pub const PUBLIC_GCP_IMAGE_PROJECT: &str = "open-ci-public-images";

/// Image tag pinned to `open-ci` v0.2.2 and `actions/runner` v2.338.2.
const PUBLIC_GCP_IMAGE_TAG: &str = "v022-r1";

fn public_gcp_image(arch: PoolArch) -> String {
    format!(
        "projects/{PUBLIC_GCP_IMAGE_PROJECT}/global/images/open-ci-ubuntu-2404-{}-{PUBLIC_GCP_IMAGE_TAG}",
        arch.name()
    )
}

fn gcp_region(args: &InitArgs) -> &str {
    args.region.as_deref().unwrap_or("us-central1")
}

fn gcp_zones(args: &InitArgs) -> Vec<String> {
    if args.zones.is_empty() {
        let region = gcp_region(args);
        ["a", "b", "c"]
            .iter()
            .map(|suffix| format!("{region}-{suffix}"))
            .collect()
    } else {
        args.zones.clone()
    }
}

fn aws_zone(args: &InitArgs, aws_region: &str) -> String {
    match (args.preset, args.zones.first()) {
        (InitPreset::AwsSpot, Some(zone)) => zone.clone(),
        _ => format!("{aws_region}a"),
    }
}

fn write_pools(out: &mut String, args: &InitArgs) -> Result<()> {
    write_pool(out, args, PoolArch::X64)?;
    if args.arm64 && args.preset != InitPreset::BareMetal {
        write_pool(out, args, PoolArch::Arm64)?;
    }
    Ok(())
}

fn write_pool(out: &mut String, args: &InitArgs, arch: PoolArch) -> Result<()> {
    let creates = args.max_instances.clamp(1, 4);
    let disk_gb = match (args.preset, arch) {
        (InitPreset::BareMetal | InitPreset::Hybrid, PoolArch::X64) => 64,
        _ => 40,
    };
    let pool = format!("linux-{}", arch.name());

    writeln!(out, "[pools.{pool}]")?;
    writeln!(out, "runner_label = \"open-ci-{pool}\"")?;
    writeln!(out, "fallback_label = \"{}\"", arch.fallback_label())?;
    writeln!(out, "architecture = \"{}\"", arch.name())?;
    writeln!(out, "vcpu = 4")?;
    writeln!(out, "disk_gb = {disk_gb}")?;
    writeln!(out, "max_instances = {}", args.max_instances)?;
    writeln!(out, "min_idle_instances = {}", args.min_idle)?;
    writeln!(out, "creates_per_tick = {creates}")?;
    writeln!(out, "lifetime_seconds = 2400")?;
    writeln!(out, "idle_seconds = 180")?;
    writeln!(out, "prefer_cheapest = true")?;
    writeln!(out, "transparent_cache = {}", args.with_cache)?;
    writeln!(out)?;

    if arch == PoolArch::X64 && matches!(args.preset, InitPreset::BareMetal | InitPreset::Hybrid) {
        writeln!(out, "[[pools.{pool}.placements]]")?;
        writeln!(out, "provider = \"cloud_hypervisor\"")?;
        writeln!(out, "region = \"local\"")?;
        writeln!(out, "zone = \"local-a\"")?;
        writeln!(out, "machine_type = \"ch-4c-16g\"")?;
        writeln!(out, "subnet = \"open-ci-br0\"")?;
        writeln!(out, "spot = false")?;
        writeln!(out)?;
    }
    if matches!(
        args.preset,
        InitPreset::GcpSpot | InitPreset::MultiCloud | InitPreset::Hybrid
    ) {
        let region = gcp_region(args);
        for zone in gcp_zones(args) {
            writeln!(out, "[[pools.{pool}.placements]]")?;
            writeln!(out, "provider = \"gcp\"")?;
            writeln!(out, "region = \"{region}\"")?;
            writeln!(out, "zone = \"{zone}\"")?;
            writeln!(out, "machine_type = \"{}\"", arch.gcp_machine_type())?;
            writeln!(
                out,
                "subnet = \"projects/{}/regions/{region}/subnetworks/open-ci-runners-{region}\"",
                args.gcp_project
            )?;
            writeln!(out, "image = \"{}\"", public_gcp_image(arch))?;
            writeln!(out, "disk_type = \"hyperdisk-balanced\"")?;
            writeln!(out, "disk_interface = \"NVME\"")?;
            writeln!(out, "nic_type = \"GVNIC\"")?;
            writeln!(out, "local_ssd_count = 1")?;
            writeln!(out, "network_bandwidth_tier = \"TIER_1\"")?;
            if let Some(tpc) = arch.gcp_threads_per_core() {
                writeln!(out, "threads_per_core = {tpc}")?;
            }
            writeln!(out, "spot = true")?;
            writeln!(out)?;
        }
    }
    if matches!(args.preset, InitPreset::AwsSpot | InitPreset::MultiCloud) {
        let aws_region = args.region.as_deref().unwrap_or("us-east-1");
        writeln!(out, "[[pools.{pool}.placements]]")?;
        writeln!(out, "provider = \"aws\"")?;
        writeln!(out, "region = \"{aws_region}\"")?;
        writeln!(out, "zone = \"{}\"", aws_zone(args, aws_region))?;
        writeln!(out, "machine_type = \"{}\"", arch.aws_machine_type())?;
        writeln!(out, "subnet = \"subnet-0123456789abcdef0\"")?;
        writeln!(out, "image = \"{}\"", arch.aws_image())?;
        writeln!(out, "security_group = \"sg-0123456789abcdef0\"")?;
        writeln!(out, "root_device = \"/dev/sda1\"")?;
        writeln!(out, "spot = true")?;
        writeln!(out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args(preset: InitPreset) -> InitArgs {
        InitArgs {
            preset,
            repo: Some("acme/app".to_owned()),
            org: None,
            public_url: "https://ci.acme.dev".to_owned(),
            gcp_project: "acme-ci-prod".to_owned(),
            aws_account: "123456789012".to_owned(),
            region: None,
            zones: Vec::new(),
            arm64: false,
            with_cache: false,
            cache_url: None,
            max_instances: 8,
            min_idle: 1,
            local_max_instances: 4,
            output: PathBuf::from("-"),
            force: false,
        }
    }

    #[test]
    fn all_presets_render_valid_configs_with_and_without_cache_and_arm64() {
        let presets = [
            InitPreset::GcpSpot,
            InitPreset::AwsSpot,
            InitPreset::MultiCloud,
            InitPreset::BareMetal,
            InitPreset::Hybrid,
        ];
        for preset in presets {
            for with_cache in [false, true] {
                for arm64 in [false, true] {
                    let mut args = base_args(preset);
                    args.with_cache = with_cache;
                    args.arm64 = arm64;
                    let rendered = render_init_config(&args);
                    assert!(
                        rendered.is_ok(),
                        "preset={preset:?} cache={with_cache} arm64={arm64} failed: {rendered:?}"
                    );
                    if let Ok(toml_text) = rendered {
                        let parsed = OpenCiConfig::parse_toml(&toml_text);
                        assert!(parsed.is_ok());
                        if let Ok(cfg) = parsed {
                            assert_eq!(cfg.cache.is_some(), with_cache);
                            if preset == InitPreset::Hybrid {
                                assert_eq!(
                                    cfg.cloud_hypervisor.as_ref().and_then(|c| c.max_instances),
                                    Some(4)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn gcp_placements<'a>(
        cfg: &'a OpenCiConfig,
        pool: &str,
    ) -> Vec<&'a open_ci_core::config::Placement> {
        cfg.pools.get(pool).map_or_else(Vec::new, |p| {
            p.placements
                .iter()
                .filter(|pl| pl.provider == open_ci_core::models::ProviderKind::Gcp)
                .collect()
        })
    }

    #[test]
    fn gcp_presets_default_to_direct_egress_multi_zone_failover_and_pinned_images() {
        for preset in [
            InitPreset::GcpSpot,
            InitPreset::MultiCloud,
            InitPreset::Hybrid,
        ] {
            let mut args = base_args(preset);
            args.arm64 = true;
            let parsed = render_init_config(&args)
                .map_err(|e| e.to_string())
                .and_then(|t| OpenCiConfig::parse_toml(&t).map_err(|e| e.to_string()));
            assert!(parsed.is_ok(), "{preset:?}: {parsed:?}");
            let Ok(cfg) = parsed else { return };
            let gcp = cfg.gcp.as_ref();
            assert!(gcp.is_some_and(|g| g.external_ipv4), "{preset:?}");
            assert!(gcp.is_some_and(|g| g.network_tags == ["open-ci-runner"]));
            assert!(gcp.is_some_and(|g| g.image == public_gcp_image(PoolArch::X64)));

            for (pool, machine, tpc, arch) in [
                ("linux-x64", "c4d-standard-8-lssd", Some(1), PoolArch::X64),
                ("linux-arm64", "c4a-standard-4-lssd", None, PoolArch::Arm64),
            ] {
                let placements = gcp_placements(&cfg, pool);
                let zones: Vec<&str> = placements.iter().map(|p| p.zone.as_str()).collect();
                assert_eq!(
                    zones,
                    ["us-central1-a", "us-central1-b", "us-central1-c"],
                    "{preset:?} {pool}"
                );
                for placement in placements {
                    assert_eq!(placement.machine_type, machine);
                    assert_eq!(placement.threads_per_core, tpc);
                    assert_eq!(placement.local_ssd_count, Some(1));
                    assert!(placement.spot);
                    assert_eq!(
                        placement.subnet,
                        "projects/acme-ci-prod/regions/us-central1/subnetworks/open-ci-runners-us-central1"
                    );
                    assert_eq!(placement.image, Some(public_gcp_image(arch)));
                }
            }
        }
    }

    #[test]
    fn zone_override_sets_gcp_failover_order_and_aws_spot_primary_zone() {
        let mut gcp = base_args(InitPreset::GcpSpot);
        gcp.region = Some("us-east1".to_owned());
        gcp.zones = vec!["us-east1-b".to_owned(), "us-east1-d".to_owned()];
        let cfg = render_init_config(&gcp)
            .ok()
            .and_then(|t| OpenCiConfig::parse_toml(&t).ok());
        let zones: Vec<String> = cfg.as_ref().map_or_else(Vec::new, |c| {
            gcp_placements(c, "linux-x64")
                .iter()
                .map(|p| p.zone.clone())
                .collect()
        });
        assert_eq!(zones, ["us-east1-b", "us-east1-d"]);

        let mut aws = base_args(InitPreset::AwsSpot);
        aws.zones = vec!["us-east-1c".to_owned()];
        let cfg = render_init_config(&aws)
            .ok()
            .and_then(|t| OpenCiConfig::parse_toml(&t).ok());
        let aws_zone = cfg
            .as_ref()
            .and_then(|c| c.pools.get("linux-x64"))
            .and_then(|p| p.placements.first())
            .map(|p| p.zone.clone());
        assert_eq!(aws_zone.as_deref(), Some("us-east-1c"));
    }

    fn packer_variable_default(hcl: &str, name: &str) -> Option<String> {
        let block = hcl.split(&format!("variable \"{name}\"")).nth(1)?;
        let line = block
            .lines()
            .find(|l| l.trim_start().starts_with("default"))?;
        Some(line.split('"').nth(1)?.to_owned())
    }

    #[test]
    fn public_images_match_release_packer_defaults_and_example_configs() {
        let hcl = include_str!("../../../../infra/packer/ubuntu-runner.pkr.hcl");
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            PUBLIC_GCP_IMAGE_TAG.starts_with(&format!("v{}-", version.replace('.', ""))),
            "publish open-ci-ubuntu-2404-<arch>-v{version}-* to {PUBLIC_GCP_IMAGE_PROJECT} and bump PUBLIC_GCP_IMAGE_TAG"
        );
        assert_eq!(
            packer_variable_default(hcl, "open_ci_version").as_deref(),
            Some(version),
            "bump open_ci_version in infra/packer/ubuntu-runner.pkr.hcl to the workspace version"
        );
        assert_eq!(
            packer_variable_default(hcl, "github_runner_version").as_deref(),
            Some("2.338.2"),
            "rebuild and republish the public images when github_runner_version changes"
        );

        let allowed: Vec<String> = [PoolArch::X64, PoolArch::Arm64]
            .into_iter()
            .map(|arch| format!("image = \"{}\"", public_gcp_image(arch)))
            .collect();
        for (path, text) in [
            (
                "config/open-ci.example.toml",
                include_str!("../../../../config/open-ci.example.toml"),
            ),
            (
                "config/open-ci.high-perf.example.toml",
                include_str!("../../../../config/open-ci.high-perf.example.toml"),
            ),
            (
                "config/open-ci.hybrid-byoc.example.toml",
                include_str!("../../../../config/open-ci.hybrid-byoc.example.toml"),
            ),
        ] {
            for line in text.lines().filter(|l| l.contains("/global/images/")) {
                assert!(
                    allowed.iter().any(|a| a == line),
                    "{path}: GCP image must be a public {PUBLIC_GCP_IMAGE_PROJECT} image: {line}"
                );
            }
            let cfg = OpenCiConfig::parse_toml(text);
            assert!(
                cfg.as_ref()
                    .is_ok_and(|c| c.gcp.as_ref().is_none_or(|g| g.external_ipv4)),
                "{path}: GCP examples must default to external_ipv4 = true direct egress"
            );
        }
    }

    #[test]
    fn org_scope_and_mutual_exclusion_validate() {
        let mut org_args = base_args(InitPreset::GcpSpot);
        org_args.repo = None;
        org_args.org = Some("acme".to_owned());
        let rendered = render_init_config(&org_args);
        assert!(rendered.is_ok());
        if let Ok(toml_text) = rendered {
            let parsed = OpenCiConfig::parse_toml(&toml_text);
            assert!(matches!(
                parsed,
                Ok(ref cfg) if cfg.organization.as_deref() == Some("acme")
            ));
        }

        let mut both = base_args(InitPreset::AwsSpot);
        both.org = Some("acme".to_owned());
        assert!(render_init_config(&both).is_err());
    }

    #[test]
    fn interactive_wizard_produces_validated_hybrid_config() {
        let input = b"hybrid\nacme\nhttps://ci.acme.dev\nacme-prod\ny\ny\n";
        let mut reader = std::io::Cursor::new(&input[..]);
        let mut sink = Vec::new();
        let base = InitArgs {
            repo: None,
            ..base_args(InitPreset::GcpSpot)
        };
        let prompted = prompt_args_from_reader(&base, &mut reader, &mut sink);
        assert!(prompted.is_ok());
        if let Ok(args) = prompted {
            assert_eq!(args.preset, InitPreset::Hybrid);
            assert_eq!(args.org.as_deref(), Some("acme"));
            assert!(args.repo.is_none());
            assert!(args.with_cache);
            assert!(args.arm64);
            assert!(render_init_config(&args).is_ok());
        }
    }
}
