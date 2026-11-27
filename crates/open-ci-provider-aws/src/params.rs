//! EC2 Query API parameter builders, state mappers, and AWS error classifiers.

use crate::xml::{self, ParsedEc2Instance};
use base64::Engine;
use open_ci_core::config::Placement;
use open_ci_provider::{CloudInstance, InstanceSpec, InstanceStatus, ProviderError};
use reqwest::StatusCode;

pub(crate) fn map_ec2_state(state: &str) -> InstanceStatus {
    match state {
        "running" => InstanceStatus::Running,
        "stopping" | "shutting-down" => InstanceStatus::Stopping,
        "terminated" | "stopped" => InstanceStatus::Terminated,
        _ => InstanceStatus::Provisioning,
    }
}

pub(crate) fn to_cloud_instance(inst: ParsedEc2Instance) -> CloudInstance {
    let name = inst
        .tags
        .get("Name")
        .cloned()
        .unwrap_or_else(|| inst.instance_id.clone());
    let pool_id = inst.tags.get("open-ci-pool").cloned().unwrap_or_default();
    CloudInstance {
        provider_id: inst.instance_id,
        name,
        pool_id,
        status: map_ec2_state(&inst.state_name),
        zone: inst.availability_zone,
    }
}

pub(crate) fn classify_aws_error(
    status: StatusCode,
    xml_body: &str,
    region: &str,
    placement: Option<&Placement>,
) -> ProviderError {
    let code = xml::parse_error_code(xml_body).unwrap_or_default();
    match code.as_str() {
        "InsufficientInstanceCapacity" | "SpotMaxPriceTooLow" | "Unsupported" => {
            ProviderError::CapacityUnavailable {
                zone: placement.map_or_else(String::new, |p| p.zone.clone()),
            }
        }
        "MaxSpotInstanceCountExceeded" | "VcpuLimitExceeded" | "InstanceLimitExceeded" => {
            ProviderError::QuotaExceeded {
                region: region.to_owned(),
                family: placement.map_or_else(String::new, |p| p.machine_family().to_owned()),
            }
        }
        "RequestLimitExceeded" | "Throttling" => ProviderError::RateLimited {
            retry_after_seconds: 30,
        },
        "UnauthorizedOperation" | "InvalidClientTokenId" | "SignatureDoesNotMatch" => {
            ProviderError::AuthFailed
        }
        c if c == "AuthFailure" || c.starts_with("AuthFailure.") => ProviderError::AuthFailed,
        "InvalidInstanceID.NotFound" | "InvalidVolume.NotFound" => ProviderError::NotFound,
        "InvalidAMIID.NotFound" | "InvalidSubnetID.NotFound" => ProviderError::InvalidConfig(code),
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            ProviderError::AuthFailed
        }
        _ => ProviderError::Transient(if code.is_empty() {
            format!("aws http {}", status.as_u16())
        } else {
            format!("aws error {code}")
        }),
    }
}

pub(crate) fn build_run_instances_params(
    spec: &InstanceSpec,
) -> Result<Vec<(&'static str, String)>, ProviderError> {
    let image = spec.placement.image.as_deref().ok_or_else(|| {
        ProviderError::InvalidConfig("aws placement requires ami image".to_owned())
    })?;
    let sg = spec.placement.security_group.as_deref().ok_or_else(|| {
        ProviderError::InvalidConfig("aws placement requires security_group".to_owned())
    })?;
    let root_device = spec.placement.root_device.as_deref().unwrap_or("/dev/sda1");
    let volume_type = spec.placement.disk_type.as_deref().unwrap_or("gp3");
    let user_data_b64 = base64::engine::general_purpose::STANDARD.encode(&spec.user_data);

    let mut params = vec![
        ("Action", "RunInstances".to_owned()),
        ("Version", "2016-11-15".to_owned()),
        ("MinCount", "1".to_owned()),
        ("MaxCount", "1".to_owned()),
        ("ImageId", image.to_owned()),
        ("InstanceType", spec.placement.machine_type.clone()),
        ("SubnetId", spec.placement.subnet.clone()),
        ("SecurityGroupId.1", sg.to_owned()),
        ("Placement.AvailabilityZone", spec.placement.zone.clone()),
        ("InstanceInitiatedShutdownBehavior", "terminate".to_owned()),
        ("ClientToken", spec.request_id.clone()),
        ("UserData", user_data_b64),
        ("BlockDeviceMapping.1.DeviceName", root_device.to_owned()),
        (
            "BlockDeviceMapping.1.Ebs.VolumeSize",
            spec.disk_gb.to_string(),
        ),
        (
            "BlockDeviceMapping.1.Ebs.VolumeType",
            volume_type.to_owned(),
        ),
        ("BlockDeviceMapping.1.Ebs.Encrypted", "true".to_owned()),
        (
            "BlockDeviceMapping.1.Ebs.DeleteOnTermination",
            "true".to_owned(),
        ),
        ("MetadataOptions.HttpTokens", "required".to_owned()),
        ("MetadataOptions.HttpPutResponseHopLimit", "1".to_owned()),
    ];

    if let (Some(cores), Some(tpc)) = (spec.placement.core_count, spec.placement.threads_per_core) {
        params.push(("CpuOptions.CoreCount", cores.to_string()));
        params.push(("CpuOptions.ThreadsPerCore", tpc.to_string()));
    }
    if let Some(iops) = spec.placement.ebs_iops {
        params.push(("BlockDeviceMapping.1.Ebs.Iops", iops.to_string()));
    }
    if let Some(mbps) = spec.placement.ebs_throughput_mbps {
        params.push(("BlockDeviceMapping.1.Ebs.Throughput", mbps.to_string()));
    }
    if let Some(group) = &spec.placement.placement_group {
        params.push(("Placement.GroupName", group.clone()));
    }

    if spec.placement.spot {
        params.push(("InstanceMarketOptions.MarketType", "spot".to_owned()));
        params.push((
            "InstanceMarketOptions.SpotOptions.SpotInstanceType",
            "one-time".to_owned(),
        ));
        params.push((
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior",
            "terminate".to_owned(),
        ));
        if let Some(max_price) = &spec.placement.max_spot_price_usd {
            params.push((
                "InstanceMarketOptions.SpotOptions.MaxPrice",
                max_price.clone(),
            ));
        }
    }

    append_resource_tags(&mut params, spec);
    Ok(params)
}

fn append_resource_tags(params: &mut Vec<(&'static str, String)>, spec: &InstanceSpec) {
    let expires = spec.expires_at_unix.to_string();
    params.extend([
        ("TagSpecification.1.ResourceType", "instance".to_owned()),
        ("TagSpecification.1.Tag.1.Key", "Name".to_owned()),
        ("TagSpecification.1.Tag.1.Value", spec.worker_name.clone()),
        ("TagSpecification.1.Tag.2.Key", "open-ci-managed".to_owned()),
        ("TagSpecification.1.Tag.2.Value", "true".to_owned()),
        ("TagSpecification.1.Tag.3.Key", "open-ci-pool".to_owned()),
        ("TagSpecification.1.Tag.3.Value", spec.pool_id.clone()),
        (
            "TagSpecification.1.Tag.4.Key",
            "open-ci-expires-at".to_owned(),
        ),
        ("TagSpecification.1.Tag.4.Value", expires.clone()),
        ("TagSpecification.2.ResourceType", "volume".to_owned()),
        ("TagSpecification.2.Tag.1.Key", "Name".to_owned()),
        ("TagSpecification.2.Tag.1.Value", spec.worker_name.clone()),
        ("TagSpecification.2.Tag.2.Key", "open-ci-managed".to_owned()),
        ("TagSpecification.2.Tag.2.Value", "true".to_owned()),
        ("TagSpecification.2.Tag.3.Key", "open-ci-pool".to_owned()),
        ("TagSpecification.2.Tag.3.Value", spec.pool_id.clone()),
        (
            "TagSpecification.2.Tag.4.Key",
            "open-ci-expires-at".to_owned(),
        ),
        ("TagSpecification.2.Tag.4.Value", expires),
    ]);
}
