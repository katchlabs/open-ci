//! `open-ci cache` — runs the native GitHub Actions v2 transparent cache server.

use crate::commands::serve::{read_optional_secret_file, wait_for_shutdown_signal};
use anyhow::{Context, Result, anyhow};
use open_ci_cache::{CacheBackendConfig, CacheConfig, CacheServer, S3Config};
use open_ci_core::{OpenCiConfig, SecretString};
use std::path::PathBuf;

pub async fn run_cache(cfg: &OpenCiConfig) -> Result<()> {
    let cache_cfg = cfg
        .cache
        .as_ref()
        .ok_or_else(|| anyhow!("missing `[cache]` section in open-ci.toml"))?;

    let signing_key = read_optional_secret_file("OPEN_CI_CACHE_SIGNING_KEY_FILE")?
        .ok_or_else(|| anyhow!("set `OPEN_CI_CACHE_SIGNING_KEY_FILE` to a >=32-byte secret"))?;

    let backend = build_cache_backend(cache_cfg)?;
    let runtime_cfg = CacheConfig {
        listen_addr: cache_cfg.listen.clone(),
        public_base_url: cache_cfg.public_url.clone(),
        signing_key,
        max_entry_bytes: cache_cfg.max_entry_bytes,
        max_total_bytes: cache_cfg.max_total_bytes,
        signed_url_ttl_seconds: cache_cfg.signed_url_ttl_seconds,
        backend,
    };

    tracing::info!(
        listen = %runtime_cfg.listen_addr,
        public_url = %runtime_cfg.public_base_url,
        "starting open-ci transparent Actions v2 cache server"
    );

    let server = CacheServer::new(runtime_cfg)
        .await
        .context("failed initializing open-ci cache server")?;
    server
        .serve_with_shutdown(wait_for_shutdown_signal())
        .await
        .context("open-ci cache server exited with error")
}

fn build_cache_backend(
    cache_cfg: &open_ci_core::config::CacheServiceConfig,
) -> Result<CacheBackendConfig> {
    if let (Some(endpoint), Some(bucket)) = (&cache_cfg.s3_endpoint, &cache_cfg.s3_bucket) {
        let access_key_id = std::env::var("OPEN_CI_CACHE_S3_ACCESS_KEY_ID")
            .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
            .map_err(|_| {
                anyhow!("set `OPEN_CI_CACHE_S3_ACCESS_KEY_ID` or `AWS_ACCESS_KEY_ID` for S3 cache")
            })?;
        let secret_access_key = std::env::var("OPEN_CI_CACHE_S3_SECRET_ACCESS_KEY")
            .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
            .map_err(|_| {
                anyhow!(
                    "set `OPEN_CI_CACHE_S3_SECRET_ACCESS_KEY` or `AWS_SECRET_ACCESS_KEY` for S3 cache"
                )
            })?;
        let session_token = std::env::var("OPEN_CI_CACHE_S3_SESSION_TOKEN")
            .or_else(|_| std::env::var("AWS_SESSION_TOKEN"))
            .ok()
            .filter(|v| !v.is_empty())
            .map(SecretString::new);

        return Ok(CacheBackendConfig::S3(S3Config {
            endpoint: endpoint.clone(),
            region: cache_cfg
                .s3_region
                .clone()
                .unwrap_or_else(|| "us-east-1".to_owned()),
            bucket: bucket.clone(),
            prefix: cache_cfg.s3_prefix.clone().unwrap_or_default(),
            access_key_id,
            secret_access_key: SecretString::new(secret_access_key),
            session_token,
        }));
    }

    let root_dir = cache_cfg
        .storage_dir
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("configure `cache.storage_dir` or S3 settings in `[cache]`"))?;
    Ok(CacheBackendConfig::LocalFs { root_dir })
}
