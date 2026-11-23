//! Native Rust GCP Compute Engine adapter for `open-ci`.
//!
//! Uses a shared `reqwest::Client` and async cached credential reader (zero
//! blocking filesystem reads on Tokio threads) with strongly-typed wire DTOs.

pub mod auth;
mod dto;

pub use auth::{
    FileTokenSource, GcpTokenSource, ImpersonatedTokenSource, ImpersonationConfig,
    MetadataTokenSource, StaticTokenSource,
};

use async_trait::async_trait;
use dto::{
    GcpDiskDto, GcpInstanceDto, GcpListDto, GcpOperationDto, build_insert_body,
    classify_http_error, classify_operation_error, map_instance_status,
};
use open_ci_core::ProviderKind;
use open_ci_core::config::{GcpProviderConfig, Placement};
use open_ci_provider::{
    CloudDisk, CloudInstance, CloudProvider, CreateReceipt, InstanceSpec, OperationRef,
    OperationStatus, ProviderError,
};
use reqwest::{Client, Method, StatusCode};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug)]
pub struct GcpProvider {
    config: GcpProviderConfig,
    client: Client,
    tokens: Arc<dyn GcpTokenSource>,
}

impl GcpProvider {
    pub fn new(config: GcpProviderConfig, client: Client, tokens: Arc<dyn GcpTokenSource>) -> Self {
        Self {
            config,
            client,
            tokens,
        }
    }

    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        placement_ctx: Option<&Placement>,
    ) -> Result<Option<T>, ProviderError> {
        let token = self.tokens.bearer_token().await?;
        let url = format!("{}{path}", self.config.compute_url);
        let mut req = self.client.request(method, url).bearer_auth(token.expose());
        if let Some(payload) = body {
            req = req.json(&payload);
        }
        let resp = req.send().await.map_err(|err| {
            if err.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Transient("gcp transport error".to_owned())
            }
        })?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::AuthFailed);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after_seconds: 30,
            });
        }
        if !status.is_success() {
            let body_json = resp.json::<Value>().await.ok();
            return Err(classify_http_error(
                status,
                body_json.as_ref(),
                placement_ctx,
            ));
        }

        let parsed = resp
            .json::<T>()
            .await
            .map_err(|_| ProviderError::Transient("invalid gcp json response".to_owned()))?;
        Ok(Some(parsed))
    }
}

fn validate_gcp_label_value(pool_id: &str) -> Result<(), ProviderError> {
    if pool_id.is_empty()
        || !pool_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ProviderError::InvalidConfig(
            "pool_id contains unsafe characters for GCP label filter".to_owned(),
        ));
    }
    Ok(())
}

fn validate_path_segment(name: &str, value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || value == "."
        || value == ".."
    {
        return Err(ProviderError::InvalidConfig(format!(
            "unsafe GCP path segment for {name}"
        )));
    }
    Ok(())
}

fn is_managed_for_pool(labels: &std::collections::BTreeMap<String, String>, pool_id: &str) -> bool {
    labels.get("open-ci-managed").is_some_and(|v| v == "true")
        && labels.get("open-ci-pool").is_some_and(|p| p == pool_id)
}

#[async_trait]
impl CloudProvider for GcpProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gcp
    }

    async fn health_check(&self, placement: &Placement) -> Result<(), ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        let path = format!("/projects/{}/zones/{}", self.config.project, placement.zone);
        let _: Option<Value> = self
            .request_json(Method::GET, &path, None, Some(placement))
            .await?;
        Ok(())
    }

    async fn get_instance(
        &self,
        placement: &Placement,
        worker_name: &str,
        _provider_instance_id: Option<&str>,
    ) -> Result<Option<CloudInstance>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_path_segment("worker_name", worker_name)?;
        let path = format!(
            "/projects/{}/zones/{}/instances/{worker_name}",
            self.config.project, placement.zone
        );
        let dto: Option<GcpInstanceDto> = self
            .request_json(Method::GET, &path, None, Some(placement))
            .await?;
        Ok(dto.map(|i| CloudInstance {
            provider_id: if i.id.is_empty() {
                i.name.clone()
            } else {
                i.id
            },
            name: i.name,
            pool_id: i.labels.get("open-ci-pool").cloned().unwrap_or_default(),
            status: map_instance_status(&i.status),
            zone: placement.zone.clone(),
        }))
    }

    async fn list_instances(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudInstance>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_gcp_label_value(pool_id)?;
        let path = format!(
            "/projects/{}/zones/{}/instances?filter=labels.open-ci-managed%3Dtrue%20AND%20labels.open-ci-pool%3D{pool_id}",
            self.config.project, placement.zone
        );
        let list: Option<GcpListDto<GcpInstanceDto>> = self
            .request_json(Method::GET, &path, None, Some(placement))
            .await?;
        Ok(list
            .map(|l| {
                l.items
                    .into_iter()
                    .filter(|i| is_managed_for_pool(&i.labels, pool_id))
                    .map(|i| CloudInstance {
                        provider_id: if i.id.is_empty() {
                            i.name.clone()
                        } else {
                            i.id
                        },
                        name: i.name,
                        pool_id: pool_id.to_owned(),
                        status: map_instance_status(&i.status),
                        zone: placement.zone.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_disks(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_gcp_label_value(pool_id)?;
        let path = format!(
            "/projects/{}/zones/{}/disks?filter=labels.open-ci-managed%3Dtrue%20AND%20labels.open-ci-pool%3D{pool_id}",
            self.config.project, placement.zone
        );
        let list: Option<GcpListDto<GcpDiskDto>> = self
            .request_json(Method::GET, &path, None, Some(placement))
            .await?;
        Ok(list
            .map(|l| {
                l.items
                    .into_iter()
                    .filter(|d| is_managed_for_pool(&d.labels, pool_id))
                    .map(|d| CloudDisk {
                        provider_id: if d.id.is_empty() {
                            d.name.clone()
                        } else {
                            d.id
                        },
                        name: d.name,
                        pool_id: pool_id.to_owned(),
                        attached: !d.users.is_empty(),
                        status: d.status,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &spec.placement.zone)?;
        validate_path_segment("request_id", &spec.request_id)?;
        let path = format!(
            "/projects/{}/zones/{}/instances?requestId={}",
            self.config.project, spec.placement.zone, spec.request_id
        );
        let body = build_insert_body(&self.config, spec);
        let op: GcpOperationDto = self
            .request_json(Method::POST, &path, Some(body), Some(&spec.placement))
            .await?
            .ok_or_else(|| ProviderError::Transient("empty gcp insert response".to_owned()))?;
        Ok(CreateReceipt {
            provider_instance_id: Some(spec.worker_name.clone()),
            operation: Some(OperationRef(op.name)),
        })
    }

    async fn poll_operation(
        &self,
        placement: &Placement,
        operation: &OperationRef,
    ) -> Result<OperationStatus, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_path_segment("operation", &operation.0)?;
        let path = format!(
            "/projects/{}/zones/{}/operations/{}",
            self.config.project, placement.zone, operation.0
        );
        let Some(op): Option<GcpOperationDto> = self
            .request_json(Method::GET, &path, None, Some(placement))
            .await?
        else {
            return Err(ProviderError::NotFound);
        };
        if op.status != "DONE" {
            return Ok(OperationStatus::Pending);
        }
        if let Some(err) = op.error {
            return Ok(OperationStatus::Failed(classify_operation_error(
                &err, placement,
            )));
        }
        Ok(OperationStatus::Done)
    }

    async fn delete_instance(
        &self,
        placement: &Placement,
        worker_name: &str,
        _provider_instance_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_path_segment("worker_name", worker_name)?;
        let path = format!(
            "/projects/{}/zones/{}/instances/{worker_name}",
            self.config.project, placement.zone
        );
        let op: Option<GcpOperationDto> = self
            .request_json(Method::DELETE, &path, None, Some(placement))
            .await?;
        Ok(op.map(|o| OperationRef(o.name)))
    }

    async fn delete_disk(
        &self,
        placement: &Placement,
        disk_name: &str,
        _provider_disk_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &placement.zone)?;
        validate_path_segment("disk_name", disk_name)?;
        let path = format!(
            "/projects/{}/zones/{}/disks/{disk_name}",
            self.config.project, placement.zone
        );
        let op: Option<GcpOperationDto> = self
            .request_json(Method::DELETE, &path, None, Some(placement))
            .await?;
        Ok(op.map(|o| OperationRef(o.name)))
    }
}
