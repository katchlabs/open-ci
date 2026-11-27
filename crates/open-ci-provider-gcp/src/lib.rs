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
use std::collections::BTreeMap;
use std::sync::Arc;

const MAX_LIST_PAGES: usize = 50;

/// Raw GCP Compute Engine instance metadata returned by `list_managed_instances`
/// (including `open-ci-expires-at` and `open-ci-tenant` labels for expiry janitors).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcpManagedInstance {
    pub id: String,
    pub name: String,
    pub status: String,
    pub creation_timestamp: Option<String>,
    pub labels: BTreeMap<String, String>,
}

/// Raw GCP Compute Engine disk metadata returned by `list_managed_disks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcpManagedDisk {
    pub id: String,
    pub name: String,
    pub status: String,
    pub attached: bool,
    pub creation_timestamp: Option<String>,
    pub labels: BTreeMap<String, String>,
}

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

    /// Lists managed GCE instances in `zone` across all pages (`nextPageToken`, bounded to 50 pages),
    /// optionally filtered by `pool_id` and `tenant_id`.
    pub async fn list_managed_instances(
        &self,
        zone: &str,
        pool_id: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Vec<GcpManagedInstance>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", zone)?;
        if let Some(pool) = pool_id {
            validate_gcp_label_value("pool_id", pool)?;
        }
        if let Some(tenant) = tenant_id.filter(|t| !t.is_empty()) {
            validate_gcp_label_value("tenant_id", tenant)?;
        }

        let filter = build_label_filter(pool_id, tenant_id);
        let base_path = format!("/projects/{}/zones/{zone}/instances", self.config.project);
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_LIST_PAGES {
            let path = build_paginated_list_path(&base_path, &filter, page_token.as_deref())?;
            let Some(list): Option<GcpListDto<GcpInstanceDto>> =
                self.request_json(Method::GET, &path, None, None).await?
            else {
                break;
            };
            for item in list.items {
                if !matches_managed_labels(&item.labels, pool_id, tenant_id) {
                    continue;
                }
                let id = if item.id.is_empty() {
                    item.name.clone()
                } else {
                    item.id
                };
                out.push(GcpManagedInstance {
                    id,
                    name: item.name,
                    status: item.status,
                    creation_timestamp: item.creation_timestamp,
                    labels: item.labels,
                });
            }
            page_token = list.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Lists managed GCE disks in `zone` across all pages (`nextPageToken`, bounded to 50 pages),
    /// optionally filtered by `pool_id` and `tenant_id`.
    pub async fn list_managed_disks(
        &self,
        zone: &str,
        pool_id: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Vec<GcpManagedDisk>, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", zone)?;
        if let Some(pool) = pool_id {
            validate_gcp_label_value("pool_id", pool)?;
        }
        if let Some(tenant) = tenant_id.filter(|t| !t.is_empty()) {
            validate_gcp_label_value("tenant_id", tenant)?;
        }

        let filter = build_label_filter(pool_id, tenant_id);
        let base_path = format!("/projects/{}/zones/{zone}/disks", self.config.project);
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_LIST_PAGES {
            let path = build_paginated_list_path(&base_path, &filter, page_token.as_deref())?;
            let Some(list): Option<GcpListDto<GcpDiskDto>> =
                self.request_json(Method::GET, &path, None, None).await?
            else {
                break;
            };
            for item in list.items {
                if !matches_managed_labels(&item.labels, pool_id, tenant_id) {
                    continue;
                }
                let id = if item.id.is_empty() {
                    item.name.clone()
                } else {
                    item.id
                };
                out.push(GcpManagedDisk {
                    id,
                    name: item.name,
                    status: item.status,
                    attached: !item.users.is_empty(),
                    creation_timestamp: item.creation_timestamp,
                    labels: item.labels,
                });
            }
            page_token = list.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

fn build_label_filter(pool_id: Option<&str>, tenant_id: Option<&str>) -> String {
    let mut clauses = vec!["labels.open-ci-managed=true".to_owned()];
    if let Some(pool) = pool_id {
        clauses.push(format!("labels.open-ci-pool={pool}"));
    }
    if let Some(tenant) = tenant_id.filter(|t| !t.is_empty()) {
        clauses.push(format!("labels.open-ci-tenant={tenant}"));
    }
    clauses.join(" AND ")
}

fn build_paginated_list_path(
    base_path: &str,
    filter: &str,
    page_token: Option<&str>,
) -> Result<String, ProviderError> {
    let mut url = reqwest::Url::parse(&format!("https://compute.invalid{base_path}"))
        .map_err(|_| ProviderError::InvalidConfig("invalid gcp list path".to_owned()))?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("filter", filter);
        if let Some(token) = page_token {
            qp.append_pair("pageToken", token);
        }
    }
    let query = url.query().unwrap_or("");
    Ok(format!("{base_path}?{query}"))
}

fn validate_gcp_label_value(field: &str, value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    {
        return Err(ProviderError::InvalidConfig(format!(
            "{field} contains unsafe characters for GCP label"
        )));
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

fn matches_managed_labels(
    labels: &BTreeMap<String, String>,
    pool_id: Option<&str>,
    tenant_id: Option<&str>,
) -> bool {
    if labels.get("open-ci-managed").is_none_or(|v| v != "true") {
        return false;
    }
    if let Some(pool) = pool_id
        && labels.get("open-ci-pool").is_none_or(|p| p != pool)
    {
        return false;
    }
    if let Some(tenant) = tenant_id.filter(|t| !t.is_empty())
        && labels.get("open-ci-tenant").is_none_or(|t| t != tenant)
    {
        return false;
    }
    true
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
        let items = self
            .list_managed_instances(&placement.zone, Some(pool_id), None)
            .await?;
        Ok(items
            .into_iter()
            .map(|i| CloudInstance {
                provider_id: i.id,
                name: i.name,
                pool_id: pool_id.to_owned(),
                status: map_instance_status(&i.status),
                zone: placement.zone.clone(),
            })
            .collect())
    }

    async fn list_disks(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError> {
        let items = self
            .list_managed_disks(&placement.zone, Some(pool_id), None)
            .await?;
        Ok(items
            .into_iter()
            .map(|d| CloudDisk {
                provider_id: d.id,
                name: d.name,
                pool_id: pool_id.to_owned(),
                attached: d.attached,
                status: d.status,
            })
            .collect())
    }

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError> {
        validate_path_segment("project", &self.config.project)?;
        validate_path_segment("zone", &spec.placement.zone)?;
        validate_path_segment("request_id", &spec.request_id)?;
        if let Some(tenant) = spec.tenant_id.as_deref().filter(|t| !t.is_empty()) {
            validate_gcp_label_value("tenant_id", tenant)?;
        }
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
