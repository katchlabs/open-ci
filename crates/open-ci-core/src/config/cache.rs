//! Configuration for the built-in GitHub Actions cache service (`open-ci cache`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const DEFAULT_ACTIONS_RESULTS_URL: &str =
    "https://results-receiver.actions.githubusercontent.com";
pub const DEFAULT_ACTIONS_TOKEN_ISSUER: &str = "https://token.actions.githubusercontent.com";
pub const DEFAULT_GCS_ENDPOINT: &str = "https://storage.googleapis.com";

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheServiceConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Base URL runners reach this service on; injected as `ACTIONS_RESULTS_URL`.
    pub public_url: String,
    #[serde(default = "default_max_entry_bytes")]
    pub max_entry_bytes: u64,
    #[serde(default = "default_signed_url_ttl_seconds")]
    pub signed_url_ttl_seconds: u64,
    #[serde(default)]
    pub hot_cache_bytes: u64,
    /// `SQLite` database holding cache entry metadata.
    #[serde(default = "default_index_path")]
    pub index_path: String,
    /// GitHub results service that receives every non-cache request (artifacts, summaries).
    #[serde(default = "default_upstream_results_url")]
    pub upstream_results_url: String,
    pub auth: CacheAuthConfig,
    #[serde(default)]
    pub storage: CacheStorageConfig,
    #[serde(default)]
    pub retention: CacheRetentionConfig,
}

/// Trust policy for the GitHub Actions runtime token (`ACTIONS_RUNTIME_TOKEN`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheAuthConfig {
    #[serde(default = "default_issuer")]
    pub issuer: String,
    /// JWKS URL; defaults to `{issuer}/.well-known/jwks`.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Required `aud` value; unchecked when absent.
    #[serde(default)]
    pub audience: Option<String>,
    pub repositories: RepositoryAllowlist,
}

/// Repositories allowed to use the cache: explicit numeric IDs, or `"any"` to opt out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RepositoryAllowlist {
    Ids(BTreeSet<u64>),
    Any(AnyRepository),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnyRepository {
    Any,
}

impl RepositoryAllowlist {
    pub fn allows(&self, repository_id: u64) -> bool {
        match self {
            Self::Ids(ids) => ids.contains(&repository_id),
            Self::Any(_) => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheStorageConfig {
    Local {
        #[serde(default = "default_local_path")]
        path: String,
    },
    S3 {
        bucket: String,
        region: String,
        /// Defaults to `https://s3.{region}.amazonaws.com`; set for R2, `MinIO`, or other S3 APIs.
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        credentials: AwsCredentialsConfig,
    },
    Gcs {
        bucket: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        credentials: GcpCredentialsConfig,
    },
}

impl Default for CacheStorageConfig {
    fn default() -> Self {
        Self::Local {
            path: default_local_path(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AwsCredentialsConfig {
    /// EC2 instance role via `IMDSv2`.
    #[default]
    Instance,
    /// `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` (R2, `MinIO`, CI).
    Environment,
    /// STS `AssumeRole` on top of the instance role.
    AssumeRole {
        role_arn: String,
        #[serde(default)]
        external_id: Option<String>,
        #[serde(default)]
        session_name: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GcpCredentialsConfig {
    /// The VM's service account via the metadata server.
    #[default]
    Metadata,
    /// IAM Credentials `generateAccessToken` for `service_account`, on top of the metadata server.
    Impersonate { service_account: String },
    /// Bearer token file rotated by an external process.
    TokenFile { path: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRetentionConfig {
    /// Least-recently-used entries are evicted above this total.
    #[serde(default = "default_max_total_bytes")]
    pub max_total_bytes: u64,
    /// Entries not restored or saved for this many days are evicted (GitHub uses 7).
    #[serde(default = "default_max_unused_days")]
    pub max_unused_days: u32,
    #[serde(default = "default_sweep_interval_seconds")]
    pub sweep_interval_seconds: u64,
}

impl Default for CacheRetentionConfig {
    fn default() -> Self {
        Self {
            max_total_bytes: default_max_total_bytes(),
            max_unused_days: default_max_unused_days(),
            sweep_interval_seconds: default_sweep_interval_seconds(),
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0:8090".to_owned()
}
const fn default_max_entry_bytes() -> u64 {
    10 * GIB
}
const fn default_signed_url_ttl_seconds() -> u64 {
    900
}
fn default_index_path() -> String {
    "/var/lib/open-ci-cache/index.db".to_owned()
}
fn default_local_path() -> String {
    "/var/lib/open-ci-cache/blobs".to_owned()
}
fn default_upstream_results_url() -> String {
    DEFAULT_ACTIONS_RESULTS_URL.to_owned()
}
fn default_issuer() -> String {
    DEFAULT_ACTIONS_TOKEN_ISSUER.to_owned()
}
const fn default_max_total_bytes() -> u64 {
    50 * GIB
}
const fn default_max_unused_days() -> u32 {
    7
}
const fn default_sweep_interval_seconds() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> Result<CacheServiceConfig, toml::de::Error> {
        toml::from_str(toml_text)
    }

    #[test]
    fn minimal_config_defaults_to_local_storage_and_github_endpoints() {
        let cfg = parse("public_url = \"http://10.0.0.5:8090\"\n[auth]\nrepositories = [42]\n");
        assert!(matches!(
            cfg,
            Ok(c) if c.storage == CacheStorageConfig::default()
                && c.auth.issuer == DEFAULT_ACTIONS_TOKEN_ISSUER
                && c.upstream_results_url == DEFAULT_ACTIONS_RESULTS_URL
                && c.auth.repositories.allows(42)
                && !c.auth.repositories.allows(43)
                && c.hot_cache_bytes == 0
        ));
        let with_hot = parse(
            "public_url = \"http://10.0.0.5:8090\"\nhot_cache_bytes = 1073741824\n[auth]\nrepositories = [42]\n",
        );
        assert!(matches!(with_hot, Ok(c) if c.hot_cache_bytes == 1_073_741_824));
    }

    #[test]
    fn any_repository_requires_the_exact_keyword() {
        let any = parse("public_url = \"http://c\"\n[auth]\nrepositories = \"any\"\n");
        assert!(matches!(any, Ok(c) if c.auth.repositories.allows(7)));
        assert!(parse("public_url = \"http://c\"\n[auth]\nrepositories = \"all\"\n").is_err());
        assert!(parse("public_url = \"http://c\"\n[auth]\n").is_err());
    }

    #[test]
    fn tagged_storage_and_credentials_parse_and_reject_unknown_fields() {
        let gcs = parse(
            "public_url = \"http://c\"\n[auth]\nrepositories = [1]\n[storage]\ntype = \"gcs\"\nbucket = \"b\"\n[storage.credentials]\ntype = \"impersonate\"\nservice_account = \"sa@p.iam.gserviceaccount.com\"\n",
        );
        assert!(matches!(
            gcs,
            Ok(c) if matches!(&c.storage, CacheStorageConfig::Gcs { credentials: GcpCredentialsConfig::Impersonate { .. }, .. })
        ));
        let s3 = parse(
            "public_url = \"http://c\"\n[auth]\nrepositories = [1]\n[storage]\ntype = \"s3\"\nbucket = \"b\"\nregion = \"us-west-2\"\n",
        );
        assert!(matches!(
            s3,
            Ok(c) if matches!(&c.storage, CacheStorageConfig::S3 { credentials: AwsCredentialsConfig::Instance, .. })
        ));
        let typo = parse(
            "public_url = \"http://c\"\n[auth]\nrepositories = [1]\n[storage]\ntype = \"s3\"\nbucket = \"b\"\nregion = \"r\"\nbuckt = \"x\"\n",
        );
        assert!(typo.is_err());
    }
}
