//! Native Rust AWS EC2 & STS adapter for `open-ci`.
//!
//! Signs Query API requests directly with AWS Signature Version 4 over `reqwest` —
//! zero Python `aws` CLI subprocesses, zero temp files, and zero synthetic GCP JSON.

pub mod credentials;
mod params;
pub mod sigv4;
pub mod xml;

pub use credentials::{
    AssumeRoleConfig, AwsCredentialChain, AwsCredentialsSource, EnvAwsCredentials,
    GceWebIdentityCredentials, ImdsAwsCredentials, StaticAwsCredentials, StsAssumeRoleCredentials,
    WebIdentityConfig,
};

use async_trait::async_trait;
use open_ci_core::config::{AwsProviderConfig, Placement};
use open_ci_core::{Clock, ProviderKind, SystemClock};
use open_ci_provider::{
    CloudDisk, CloudInstance, CloudProvider, CreateReceipt, InstanceSpec, OperationRef,
    OperationStatus, ProviderError,
};
use params::{build_run_instances_params, classify_aws_error, to_cloud_instance};
use reqwest::Client;
use sigv4::{form_encode, sign_post};
use std::sync::Arc;
use xml::{ParsedEbsVolume, ParsedEc2Instance};

#[derive(Debug)]
pub struct AwsProvider {
    config: AwsProviderConfig,
    client: Client,
    creds: Arc<dyn AwsCredentialsSource>,
    clock: Arc<dyn Clock>,
}

impl AwsProvider {
    pub fn new(
        config: AwsProviderConfig,
        client: Client,
        creds: Arc<dyn AwsCredentialsSource>,
    ) -> Self {
        Self::with_clock(config, client, creds, Arc::new(SystemClock))
    }

    pub fn with_clock(
        config: AwsProviderConfig,
        client: Client,
        creds: Arc<dyn AwsCredentialsSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            client,
            creds,
            clock,
        }
    }

    fn endpoint_for(
        &self,
        service: &str,
        region: &str,
    ) -> Result<(String, String, String), ProviderError> {
        if let Some(override_url) = &self.config.endpoint_url {
            let parsed = reqwest::Url::parse(override_url)
                .map_err(|_| ProviderError::InvalidConfig("invalid aws endpoint_url".to_owned()))?;
            let host = match (parsed.host_str(), parsed.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_owned(),
                _ => {
                    return Err(ProviderError::InvalidConfig(
                        "missing host in aws endpoint_url".to_owned(),
                    ));
                }
            };
            let path = parsed.path().to_owned();
            return Ok((override_url.clone(), host, path));
        }
        if region.is_empty()
            || region.len() > 32
            || !region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ProviderError::InvalidConfig(
                "invalid aws region".to_owned(),
            ));
        }
        let host = format!("{service}.{region}.amazonaws.com");
        let url = format!("https://{host}/");
        Ok((url, host, "/".to_owned()))
    }

    async fn call_query(
        &self,
        service: &str,
        region: &str,
        params: &[(&str, String)],
        placement: Option<&Placement>,
    ) -> Result<String, ProviderError> {
        let creds = self.creds.credentials().await?;
        let (url, host, path) = self.endpoint_for(service, region)?;
        let body = form_encode(params);
        let signed = sign_post(
            &creds,
            region,
            service,
            &host,
            &path,
            &body,
            self.clock.now_unix(),
        )
        .map_err(ProviderError::Transient)?;

        let mut req = self
            .client
            .post(&url)
            .header(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization)
            .body(body);

        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token.expose());
        }

        let resp = req.send().await.map_err(|err| {
            if err.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Transient("aws transport error".to_owned())
            }
        })?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|_| ProviderError::Transient("failed reading aws response".to_owned()))?;

        if status.is_success() {
            return Ok(text);
        }

        Err(classify_aws_error(status, &text, region, placement))
    }

    /// Lists raw EC2 instances managed by `open-ci` in `region` (used by the Rust expiry janitor).
    pub async fn describe_managed_instances(
        &self,
        region: &str,
        pool_id: Option<&str>,
    ) -> Result<Vec<ParsedEc2Instance>, ProviderError> {
        let mut params = vec![
            ("Action", "DescribeInstances".to_owned()),
            ("Version", "2016-11-15".to_owned()),
            ("Filter.1.Name", "tag:open-ci-managed".to_owned()),
            ("Filter.1.Value.1", "true".to_owned()),
        ];
        if let Some(pool) = pool_id {
            params.push(("Filter.2.Name", "tag:open-ci-pool".to_owned()));
            params.push(("Filter.2.Value.1", pool.to_owned()));
        }
        let xml = self.call_query("ec2", region, &params, None).await?;
        Ok(xml::parse_instances(&xml))
    }

    /// Lists raw EBS volumes managed by `open-ci` in `region` (used by the Rust expiry janitor).
    pub async fn describe_managed_volumes(
        &self,
        region: &str,
        pool_id: Option<&str>,
    ) -> Result<Vec<ParsedEbsVolume>, ProviderError> {
        let mut params = vec![
            ("Action", "DescribeVolumes".to_owned()),
            ("Version", "2016-11-15".to_owned()),
            ("Filter.1.Name", "tag:open-ci-managed".to_owned()),
            ("Filter.1.Value.1", "true".to_owned()),
        ];
        if let Some(pool) = pool_id {
            params.push(("Filter.2.Name", "tag:open-ci-pool".to_owned()));
            params.push(("Filter.2.Value.1", pool.to_owned()));
        }
        let xml = self.call_query("ec2", region, &params, None).await?;
        Ok(xml::parse_volumes(&xml))
    }

    /// Terminates a single EC2 instance by ID.
    pub async fn terminate_instance_by_id(
        &self,
        region: &str,
        instance_id: &str,
    ) -> Result<(), ProviderError> {
        let params = [
            ("Action", "TerminateInstances".to_owned()),
            ("Version", "2016-11-15".to_owned()),
            ("InstanceId.1", instance_id.to_owned()),
        ];
        let _ = self.call_query("ec2", region, &params, None).await?;
        Ok(())
    }

    /// Deletes a single detached EBS volume by ID.
    pub async fn delete_volume_by_id(
        &self,
        region: &str,
        volume_id: &str,
    ) -> Result<(), ProviderError> {
        let params = [
            ("Action", "DeleteVolume".to_owned()),
            ("Version", "2016-11-15".to_owned()),
            ("VolumeId", volume_id.to_owned()),
        ];
        let _ = self.call_query("ec2", region, &params, None).await?;
        Ok(())
    }
}

#[async_trait]
impl CloudProvider for AwsProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Aws
    }

    async fn health_check(&self, placement: &Placement) -> Result<(), ProviderError> {
        let params = [
            ("Action", "GetCallerIdentity".to_owned()),
            ("Version", "2011-06-15".to_owned()),
        ];
        let xml = self
            .call_query("sts", &placement.region, &params, Some(placement))
            .await?;
        let account = xml::parse_sts_account(&xml).unwrap_or_default();
        if account != self.config.account_id {
            return Err(ProviderError::AuthFailed);
        }
        Ok(())
    }

    async fn get_instance(
        &self,
        placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<CloudInstance>, ProviderError> {
        let mut params = vec![
            ("Action", "DescribeInstances".to_owned()),
            ("Version", "2016-11-15".to_owned()),
        ];
        if let Some(id) = provider_instance_id {
            params.push(("InstanceId.1", id.to_owned()));
        } else {
            params.push(("Filter.1.Name", "tag:Name".to_owned()));
            params.push(("Filter.1.Value.1", worker_name.to_owned()));
            params.push(("Filter.2.Name", "tag:open-ci-managed".to_owned()));
            params.push(("Filter.2.Value.1", "true".to_owned()));
        }
        match self
            .call_query("ec2", &placement.region, &params, Some(placement))
            .await
        {
            Ok(xml) => Ok(xml::parse_instances(&xml)
                .into_iter()
                .find(|i| i.state_name != "terminated")
                .map(to_cloud_instance)),
            Err(ProviderError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn list_instances(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudInstance>, ProviderError> {
        let instances = self
            .describe_managed_instances(&placement.region, Some(pool_id))
            .await?;
        Ok(instances
            .into_iter()
            .filter(|i| i.state_name != "terminated" && i.availability_zone == placement.zone)
            .map(to_cloud_instance)
            .collect())
    }

    async fn list_disks(
        &self,
        placement: &Placement,
        pool_id: &str,
    ) -> Result<Vec<CloudDisk>, ProviderError> {
        let volumes = self
            .describe_managed_volumes(&placement.region, Some(pool_id))
            .await?;
        Ok(volumes
            .into_iter()
            .map(|v| CloudDisk {
                name: v
                    .tags
                    .get("Name")
                    .cloned()
                    .unwrap_or_else(|| v.volume_id.clone()),
                provider_id: v.volume_id,
                pool_id: pool_id.to_owned(),
                attached: v.attached,
                status: v.status,
            })
            .collect())
    }

    async fn create_instance(&self, spec: &InstanceSpec) -> Result<CreateReceipt, ProviderError> {
        let params = build_run_instances_params(spec)?;
        let xml = self
            .call_query(
                "ec2",
                &spec.placement.region,
                &params,
                Some(&spec.placement),
            )
            .await?;
        let created = xml::parse_instances(&xml)
            .into_iter()
            .next()
            .ok_or_else(|| {
                ProviderError::Transient("missing instanceId in RunInstances".to_owned())
            })?;

        Ok(CreateReceipt {
            provider_instance_id: Some(created.instance_id),
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
        placement: &Placement,
        worker_name: &str,
        provider_instance_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        let target_id = if let Some(id) = provider_instance_id {
            Some(id.to_owned())
        } else {
            self.get_instance(placement, worker_name, None)
                .await?
                .map(|i| i.provider_id)
        };
        let Some(instance_id) = target_id else {
            return Ok(None);
        };
        match self
            .terminate_instance_by_id(&placement.region, &instance_id)
            .await
        {
            Ok(()) | Err(ProviderError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn delete_disk(
        &self,
        placement: &Placement,
        _disk_name: &str,
        provider_disk_id: Option<&str>,
    ) -> Result<Option<OperationRef>, ProviderError> {
        let Some(volume_id) = provider_disk_id else {
            return Ok(None);
        };
        match self.delete_volume_by_id(&placement.region, volume_id).await {
            Ok(()) | Err(ProviderError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }
}
