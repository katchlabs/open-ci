//! Provider-specific configuration validation tests for `OpenCiConfig::parse_toml`.

use open_ci_core::{ConfigError, OpenCiConfig};

fn parse_aws(extra: &str) -> Result<OpenCiConfig, ConfigError> {
    let toml = format!(
        r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[aws]
account_id = "123456789012"

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 8
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "aws"
region = "us-east-1"
zone = "us-east-1a"
machine_type = "c7i.4xlarge"
subnet = "subnet-0123456789abcdef0"
image = "ami-0123456789abcdef0"
security_group = "sg-0123456789abcdef0"
{extra}
"#
    );
    OpenCiConfig::parse_toml(&toml)
}

fn parse_gcp(extra: &str) -> Result<OpenCiConfig, ConfigError> {
    let toml = format!(
        r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[gcp]
project = "demo-proj"
image = "projects/demo-proj/global/images/runner-v1"

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 8
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "gcp"
region = "us-central1"
zone = "us-central1-a"
machine_type = "c3-standard-8"
subnet = "projects/demo-proj/regions/us-central1/subnetworks/ci"
{extra}
"#
    );
    OpenCiConfig::parse_toml(&toml)
}

fn parse_ch(ch_extra: &str, placement_extra: &str) -> Result<OpenCiConfig, ConfigError> {
    let toml = format!(
        r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[cloud_hypervisor]
kernel_path = "/var/lib/open-ci/vmlinux"
rootfs_image = "/var/lib/open-ci/runner.raw"
{ch_extra}

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 4
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "cloud_hypervisor"
region = "local"
zone = "local-1"
machine_type = "ch-4c-16g"
subnet = "open-ci-br0"
{placement_extra}
"#
    );
    OpenCiConfig::parse_toml(&toml)
}

fn invalid_field(res: &Result<OpenCiConfig, ConfigError>) -> Option<&'static str> {
    match res {
        Err(ConfigError::Invalid { field, .. }) => Some(field),
        _ => None,
    }
}

#[test]
fn aws_placement_hardware_fields_validate() {
    assert!(parse_aws("").is_ok());
    assert!(
        parse_aws(
            r#"
core_count = 8
threads_per_core = 1
ebs_iops = 3000
ebs_throughput_mbps = 125
placement_group = "ci-cluster-01"
"#
        )
        .is_ok()
    );
    assert!(
        parse_aws(
            r"
core_count = 4
threads_per_core = 2
ebs_iops = 64000
ebs_throughput_mbps = 1000
"
        )
        .is_ok()
    );

    let long_group = format!("placement_group = \"{}\"", "a".repeat(256));
    let invalid_cases = [
        (
            "core_count = 0\nthreads_per_core = 1",
            "pools.placements.core_count",
        ),
        ("core_count = 8", "pools.placements.core_count"),
        ("threads_per_core = 1", "pools.placements.core_count"),
        (
            "core_count = 8\nthreads_per_core = 0",
            "pools.placements.threads_per_core",
        ),
        (
            "core_count = 8\nthreads_per_core = 3",
            "pools.placements.threads_per_core",
        ),
        ("local_ssd_count = 1", "pools.placements.local_ssd_count"),
        (
            "network_bandwidth_tier = \"TIER_1\"",
            "pools.placements.network_bandwidth_tier",
        ),
        ("ebs_iops = 2999", "pools.placements.ebs_iops"),
        ("ebs_iops = 64001", "pools.placements.ebs_iops"),
        (
            "ebs_throughput_mbps = 124",
            "pools.placements.ebs_throughput_mbps",
        ),
        (
            "ebs_throughput_mbps = 1001",
            "pools.placements.ebs_throughput_mbps",
        ),
        ("placement_group = \"\"", "pools.placements.placement_group"),
        (
            "placement_group = \"has space\"",
            "pools.placements.placement_group",
        ),
        (long_group.as_str(), "pools.placements.placement_group"),
    ];
    for (extra, expected_field) in invalid_cases {
        assert_eq!(
            invalid_field(&parse_aws(extra)),
            Some(expected_field),
            "aws case: {extra}"
        );
    }
}

#[test]
fn gcp_placement_hardware_fields_validate() {
    assert!(parse_gcp("").is_ok());
    assert!(
        parse_gcp(
            r#"
threads_per_core = 1
local_ssd_count = 1
network_bandwidth_tier = "TIER_1"
"#
        )
        .is_ok()
    );
    assert!(
        parse_gcp(
            r#"
threads_per_core = 2
local_ssd_count = 24
network_bandwidth_tier = "DEFAULT"
"#
        )
        .is_ok()
    );

    let invalid_cases = [
        ("threads_per_core = 0", "pools.placements.threads_per_core"),
        ("threads_per_core = 3", "pools.placements.threads_per_core"),
        ("core_count = 4", "pools.placements.core_count"),
        ("ebs_iops = 3000", "pools.placements.ebs_iops"),
        (
            "ebs_throughput_mbps = 250",
            "pools.placements.ebs_throughput_mbps",
        ),
        (
            "placement_group = \"ci-pg\"",
            "pools.placements.placement_group",
        ),
        ("local_ssd_count = 0", "pools.placements.local_ssd_count"),
        ("local_ssd_count = 25", "pools.placements.local_ssd_count"),
        (
            "network_bandwidth_tier = \"ULTRA\"",
            "pools.placements.network_bandwidth_tier",
        ),
    ];
    for (extra, expected_field) in invalid_cases {
        assert_eq!(
            invalid_field(&parse_gcp(extra)),
            Some(expected_field),
            "gcp case: {extra}"
        );
    }
}

#[test]
fn cloud_hypervisor_config_and_placements_validate() {
    assert!(parse_ch("", "").is_ok());
    assert!(parse_ch("max_instances = 4", "").is_ok());
    assert!(parse_ch("", "threads_per_core = 1").is_ok());

    let invalid_ch_cases = [
        ("binary = \"\"", "cloud_hypervisor.binary"),
        ("state_dir = \"\"", "cloud_hypervisor.state_dir"),
        ("bridge_name = \"\"", "cloud_hypervisor.bridge_name"),
        (
            "subnet_cidr = \"10.200.0.0\"",
            "cloud_hypervisor.subnet_cidr",
        ),
        (
            "virtio_blk_queues = 0",
            "cloud_hypervisor.virtio_blk_queues",
        ),
        (
            "virtio_blk_queues = 17",
            "cloud_hypervisor.virtio_blk_queues",
        ),
        ("default_vcpus = 0", "cloud_hypervisor.default_vcpus"),
        ("default_vcpus = 129", "cloud_hypervisor.default_vcpus"),
        (
            "default_memory_mib = 256",
            "cloud_hypervisor.default_memory_mib",
        ),
        (
            "default_memory_mib = 524289",
            "cloud_hypervisor.default_memory_mib",
        ),
        ("max_instances = 0", "cloud_hypervisor.max_instances"),
    ];
    for (extra, expected_field) in invalid_ch_cases {
        assert_eq!(
            invalid_field(&parse_ch(extra, "")),
            Some(expected_field),
            "cloud_hypervisor section case: {extra}"
        );
    }

    let invalid_placement_cases = [
        ("core_count = 4", "pools.placements.core_count"),
        ("local_ssd_count = 1", "pools.placements.local_ssd_count"),
        (
            "network_bandwidth_tier = \"TIER_1\"",
            "pools.placements.network_bandwidth_tier",
        ),
        ("ebs_iops = 3000", "pools.placements.ebs_iops"),
        (
            "ebs_throughput_mbps = 250",
            "pools.placements.ebs_throughput_mbps",
        ),
        (
            "placement_group = \"ci-pg\"",
            "pools.placements.placement_group",
        ),
    ];
    for (extra, expected_field) in invalid_placement_cases {
        assert_eq!(
            invalid_field(&parse_ch("", extra)),
            Some(expected_field),
            "cloud_hypervisor placement case: {extra}"
        );
    }

    let missing_section = OpenCiConfig::parse_toml(
        r#"
repository = "acme/app"

[server]
listen = "127.0.0.1:8080"
public_url = "https://ci.example.com"

[github]

[admission]
mode = "webhook"

[admission.capacity]
vcpu = 64
instances = 16
disk_gb = 1000

[pools.fast]
runner_label = "open-ci-fast"
fallback_label = "ubuntu-latest"
architecture = "x64"
vcpu = 4
disk_gb = 50
max_instances = 4

[[pools.fast.placements]]
provider = "cloud_hypervisor"
region = "local"
zone = "local-1"
machine_type = "ch-4c-16g"
subnet = "open-ci-br0"
"#,
    );
    assert_eq!(invalid_field(&missing_section), Some("cloud_hypervisor"));
}
