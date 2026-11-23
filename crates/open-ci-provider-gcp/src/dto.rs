//! Strongly-typed GCP Compute Engine wire DTOs, payload builders, and error classifiers.

use open_ci_core::config::{GcpProviderConfig, Placement};
use open_ci_provider::{InstanceSpec, InstanceStatus, ProviderError};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub(crate) struct GcpInstanceDto {
    #[serde(default)]
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
pub(crate) struct GcpDiskDto {
    #[serde(default)]
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) users: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct GcpListDto<T> {
    #[serde(default = "Vec::new")]
    pub(crate) items: Vec<T>,
}

#[derive(Deserialize)]
pub(crate) struct GcpOperationDto {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) error: Option<GcpOpErrorDto>,
}

#[derive(Deserialize)]
pub(crate) struct GcpOpErrorDto {
    #[serde(default)]
    pub(crate) errors: Vec<GcpOpErrorItem>,
}

#[derive(Deserialize)]
pub(crate) struct GcpOpErrorItem {
    #[serde(default)]
    pub(crate) code: String,
    #[serde(default, rename = "errorDetails")]
    pub(crate) error_details: Vec<GcpErrorDetail>,
}

#[derive(Deserialize)]
pub(crate) struct GcpErrorDetail {
    #[serde(default, rename = "quotaInfo")]
    pub(crate) quota_info: Option<GcpQuotaInfo>,
}

#[derive(Deserialize)]
pub(crate) struct GcpQuotaInfo {
    #[serde(default)]
    pub(crate) dimensions: BTreeMap<String, String>,
}

pub(crate) fn build_insert_body(config: &GcpProviderConfig, spec: &InstanceSpec) -> Value {
    let family = spec.placement.machine_family();
    let modern_platform = matches!(
        family,
        "c3" | "c3d" | "c4" | "c4a" | "c4d" | "n4" | "n4d" | "h3" | "m3" | "a3" | "g2"
    );
    let default_disk = if matches!(family, "c4" | "c4a" | "c4d" | "n4" | "n4d") {
        "hyperdisk-balanced"
    } else {
        "pd-balanced"
    };
    let disk_type = spec.placement.disk_type.as_deref().unwrap_or(default_disk);
    let disk_interface = spec
        .placement
        .disk_interface
        .as_deref()
        .unwrap_or(if modern_platform { "NVME" } else { "SCSI" });
    let image = spec.placement.image.as_deref().unwrap_or(&config.image);

    let mut net = json!({ "subnetwork": spec.placement.subnet });
    if let Some(nic_type) = &spec.placement.nic_type {
        net["nicType"] = json!(nic_type);
    } else if modern_platform {
        net["nicType"] = json!("GVNIC");
    }
    if config.external_ipv4 {
        net["accessConfigs"] = json!([{
            "name": "External NAT",
            "type": "ONE_TO_ONE_NAT",
            "networkTier": config.network_tier,
        }]);
    }

    let mut labels = BTreeMap::new();
    labels.insert("open-ci-managed".to_owned(), "true".to_owned());
    labels.insert("open-ci-pool".to_owned(), spec.pool_id.clone());

    let provisioning_model = if spec.placement.spot {
        "SPOT"
    } else {
        "STANDARD"
    };

    let mut body = json!({
        "name": spec.worker_name,
        "machineType": format!("zones/{}/machineTypes/{}", spec.placement.zone, spec.placement.machine_type),
        "labels": labels,
        "disks": [{
            "boot": true,
            "autoDelete": true,
            "interface": disk_interface,
            "initializeParams": {
                "diskName": spec.worker_name,
                "sourceImage": image,
                "diskSizeGb": spec.disk_gb.to_string(),
                "diskType": format!("zones/{}/diskTypes/{disk_type}", spec.placement.zone),
                "labels": labels,
            }
        }],
        "networkInterfaces": [net],
        "serviceAccounts": [],
        "shieldedInstanceConfig": {
            "enableSecureBoot": true,
            "enableVtpm": true,
            "enableIntegrityMonitoring": true,
        },
        "scheduling": {
            "provisioningModel": provisioning_model,
            "instanceTerminationAction": "DELETE",
            "automaticRestart": false,
            "onHostMaintenance": "TERMINATE",
            "maxRunDuration": { "seconds": spec.lifetime_seconds.to_string() },
        },
        "metadata": {
            "items": [
                {
                    "key": "block-project-ssh-keys",
                    "value": "true",
                },
                {
                    "key": "startup-script",
                    "value": spec.user_data,
                }
            ]
        }
    });

    if !config.network_tags.is_empty() {
        body["tags"] = json!({ "items": config.network_tags });
    }
    body
}

pub(crate) fn map_instance_status(status: &str) -> InstanceStatus {
    match status {
        "RUNNING" => InstanceStatus::Running,
        "STOPPING" | "SUSPENDING" => InstanceStatus::Stopping,
        "TERMINATED" | "STOPPED" | "SUSPENDED" => InstanceStatus::Terminated,
        _ => InstanceStatus::Provisioning,
    }
}

pub(crate) fn classify_http_error(
    status: StatusCode,
    body: Option<&Value>,
    placement: Option<&Placement>,
) -> ProviderError {
    let reason = body
        .and_then(|v| v["error"]["errors"][0]["reason"].as_str())
        .unwrap_or("");
    match reason {
        "quotaExceeded" => ProviderError::QuotaExceeded {
            region: placement.map_or_else(String::new, |p| p.region.clone()),
            family: placement.map_or_else(String::new, |p| p.machine_family().to_owned()),
        },
        "zoneResourcePoolExhausted" | "ZONE_RESOURCE_POOL_EXHAUSTED" => {
            ProviderError::CapacityUnavailable {
                zone: placement.map_or_else(String::new, |p| p.zone.clone()),
            }
        }
        "rateLimitExceeded" | "userRateLimitExceeded" => ProviderError::RateLimited {
            retry_after_seconds: 30,
        },
        _ => ProviderError::Transient(format!("gcp http {}", status.as_u16())),
    }
}

pub(crate) fn classify_operation_error(
    err: &GcpOpErrorDto,
    placement: &Placement,
) -> ProviderError {
    for item in &err.errors {
        match item.code.as_str() {
            "QUOTA_EXCEEDED" => {
                let dims = item
                    .error_details
                    .iter()
                    .find_map(|d| d.quota_info.as_ref())
                    .map(|q| &q.dimensions);
                let region = dims
                    .and_then(|d| d.get("region"))
                    .cloned()
                    .unwrap_or_else(|| placement.region.clone());
                let family = dims
                    .and_then(|d| d.get("vm_family"))
                    .cloned()
                    .unwrap_or_else(|| placement.machine_family().to_owned());
                return ProviderError::QuotaExceeded { region, family };
            }
            "ZONE_RESOURCE_POOL_EXHAUSTED" | "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS" => {
                return ProviderError::CapacityUnavailable {
                    zone: placement.zone.clone(),
                };
            }
            _ => {}
        }
    }
    ProviderError::Transient("gcp operation failed".to_owned())
}
