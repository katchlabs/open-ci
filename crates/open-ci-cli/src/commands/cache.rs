//! `open-ci cache` — runs the GitHub Actions Cache Service v2 server.

use crate::commands::serve::{read_optional_secret_file, wait_for_shutdown_signal};
use anyhow::{Context, Result, anyhow};
use open_ci_cache::{
    BlobStore, CacheConfig, CacheIndex, CacheServer, GcsBlobStore, GcsStoreConfig, LocalBlobStore,
    RetentionPolicy, S3BlobStore, S3StoreConfig,
};
use open_ci_core::config::{
    AwsCredentialsConfig, CacheServiceConfig, CacheStorageConfig, DEFAULT_GCS_ENDPOINT,
    GcpCredentialsConfig,
};
use open_ci_core::{OpenCiConfig, SystemClock};
use open_ci_github::{RuntimeTokenPolicy, RuntimeTokenVerifier};
use open_ci_provider_aws::{
    AssumeRoleConfig, AwsCredentialsSource, EnvAwsCredentials, ImdsAwsCredentials,
    StsAssumeRoleCredentials,
};
use open_ci_provider_gcp::{
    FileTokenSource, GcpTokenSource, ImpersonatedTokenSource, ImpersonationConfig,
    MetadataTokenSource,
};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const GCS_READ_WRITE_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const TOKEN_FILE_REFRESH: Duration = Duration::from_secs(60);
const STORAGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds each read, not the whole transfer, so multi-GiB archives still stream while a stalled
/// storage or token endpoint fails instead of hanging requests and the retention sweep.
const STORAGE_READ_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run_cache(cfg: &OpenCiConfig) -> Result<()> {
    let cache = cfg
        .cache
        .as_ref()
        .ok_or_else(|| anyhow!("missing `[cache]` section in open-ci.toml"))?;
    let signing_key = read_optional_secret_file("OPEN_CI_CACHE_SIGNING_KEY_FILE")?
        .ok_or_else(|| anyhow!("set `OPEN_CI_CACHE_SIGNING_KEY_FILE` to a >=32-byte secret"))?;
    let http = Client::builder()
        .connect_timeout(STORAGE_CONNECT_TIMEOUT)
        .read_timeout(STORAGE_READ_TIMEOUT)
        .build()
        .context("failed building HTTP client")?;

    let store = build_store(&cache.storage, &http).await?;
    let index = CacheIndex::open(Path::new(&cache.index_path))
        .await
        .context("failed opening cache index")?;
    let verifier = RuntimeTokenVerifier::new(
        RuntimeTokenPolicy {
            issuer: cache.auth.issuer.clone(),
            jwks_url: cache.auth.jwks_url.clone(),
            audience: cache.auth.audience.clone(),
            repositories: cache.auth.repositories.clone(),
        },
        http,
    );
    let config = runtime_config(cache, signing_key);

    tracing::info!(
        listen = %config.listen_addr,
        public_url = %config.public_base_url,
        storage = storage_kind(&cache.storage),
        "starting open-ci cache server"
    );
    CacheServer::new(config, store, index, verifier, Arc::new(SystemClock))
        .context("invalid cache configuration")?
        .serve_with_shutdown(wait_for_shutdown_signal())
        .await
        .context("open-ci cache server exited with error")
}

fn runtime_config(
    cache: &CacheServiceConfig,
    signing_key: open_ci_core::SecretString,
) -> CacheConfig {
    CacheConfig {
        listen_addr: cache.listen.clone(),
        public_base_url: cache.public_url.clone(),
        signing_key,
        max_entry_bytes: cache.max_entry_bytes,
        signed_url_ttl_seconds: cache.signed_url_ttl_seconds,
        upstream_results_url: cache.upstream_results_url.clone(),
        retention: RetentionPolicy {
            max_total_bytes: cache.retention.max_total_bytes,
            max_unused_days: cache.retention.max_unused_days,
        },
        sweep_interval: Duration::from_secs(cache.retention.sweep_interval_seconds),
    }
}

async fn build_store(storage: &CacheStorageConfig, http: &Client) -> Result<Arc<dyn BlobStore>> {
    Ok(match storage {
        CacheStorageConfig::Local { path } => Arc::new(
            LocalBlobStore::open(PathBuf::from(path))
                .await
                .context("failed opening local cache storage")?,
        ),
        CacheStorageConfig::S3 {
            bucket,
            region,
            endpoint,
            prefix,
            credentials,
        } => Arc::new(S3BlobStore::new(
            S3StoreConfig {
                endpoint: endpoint
                    .clone()
                    .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
                region: region.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
            aws_credentials(credentials, region, http),
            http.clone(),
        )?),
        CacheStorageConfig::Gcs {
            bucket,
            endpoint,
            prefix,
            credentials,
        } => Arc::new(GcsBlobStore::new(
            GcsStoreConfig {
                endpoint: endpoint
                    .clone()
                    .unwrap_or_else(|| DEFAULT_GCS_ENDPOINT.to_owned()),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
            gcp_tokens(credentials, http),
            http.clone(),
        )?),
    })
}

fn aws_credentials(
    credentials: &AwsCredentialsConfig,
    region: &str,
    http: &Client,
) -> Arc<dyn AwsCredentialsSource> {
    match credentials {
        AwsCredentialsConfig::Instance => Arc::new(ImdsAwsCredentials::new(http.clone())),
        AwsCredentialsConfig::Environment => Arc::new(EnvAwsCredentials),
        AwsCredentialsConfig::AssumeRole {
            role_arn,
            external_id,
            session_name,
        } => {
            let mut role = AssumeRoleConfig::new(
                role_arn.clone(),
                session_name
                    .clone()
                    .unwrap_or_else(|| "open-ci-cache".to_owned()),
            )
            .with_region(region);
            if let Some(external_id) = external_id {
                role = role.with_external_id(external_id.clone());
            }
            Arc::new(StsAssumeRoleCredentials::new(
                Arc::new(ImdsAwsCredentials::new(http.clone())),
                http.clone(),
                role,
            ))
        }
    }
}

fn gcp_tokens(credentials: &GcpCredentialsConfig, http: &Client) -> Arc<dyn GcpTokenSource> {
    match credentials {
        GcpCredentialsConfig::Metadata => Arc::new(MetadataTokenSource::new(http.clone())),
        GcpCredentialsConfig::Impersonate { service_account } => {
            Arc::new(ImpersonatedTokenSource::new(
                Arc::new(MetadataTokenSource::new(http.clone())),
                http.clone(),
                ImpersonationConfig::new(service_account.clone())
                    .with_scopes(vec![GCS_READ_WRITE_SCOPE.to_owned()]),
            ))
        }
        GcpCredentialsConfig::TokenFile { path } => Arc::new(FileTokenSource::new(
            PathBuf::from(path),
            TOKEN_FILE_REFRESH,
        )),
    }
}

fn storage_kind(storage: &CacheStorageConfig) -> &'static str {
    match storage {
        CacheStorageConfig::Local { .. } => "local",
        CacheStorageConfig::S3 { .. } => "s3",
        CacheStorageConfig::Gcs { .. } => "gcs",
    }
}
