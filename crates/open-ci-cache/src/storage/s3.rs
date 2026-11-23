//! Lightweight pure-Rust `SigV4` S3-compatible storage backend.
//!
//! Works with AWS S3, Cloudflare R2, `MinIO`, and GCS S3-interop without heavy SDK dependencies.

use crate::auth::{validate_blob_id, validate_block_id};
use crate::config::S3Config;
use crate::error::CacheError;
use crate::index::{CacheEntry, decode_jsonl, encode_jsonl};
use crate::storage::{BlobPayload, CacheStorage, hash_block_id};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use hmac::{Hmac, Mac};
use open_ci_core::{Clock, SystemClock};
use reqwest::{Client, Method, StatusCode};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;

const S3_SERVICE: &str = "s3";

/// Pure-Rust AWS `SigV4` S3-compatible object storage backend.
#[derive(Debug)]
pub struct S3CompatibleStorage {
    config: S3Config,
    host: String,
    base_url: String,
    normalized_prefix: String,
    client: Client,
    clock: Arc<dyn Clock>,
}

#[derive(Debug)]
struct SignedS3Headers {
    amz_date: String,
    payload_sha256: String,
    authorization: String,
    security_token: Option<String>,
}

impl S3CompatibleStorage {
    /// Creates an [`S3CompatibleStorage`] using the system wall clock.
    pub fn from_config(config: S3Config) -> Result<Self, CacheError> {
        Self::new(config, Arc::new(SystemClock))
    }

    /// Creates an [`S3CompatibleStorage`] with an injectable [`Clock`] (ideal for deterministic tests).
    pub fn new(config: S3Config, clock: Arc<dyn Clock>) -> Result<Self, CacheError> {
        config.validate()?;
        let base_url = config.endpoint.trim().trim_end_matches('/').to_owned();
        let host = extract_host(&base_url)?;
        let trimmed_prefix = config.prefix.trim().trim_matches('/');
        let normalized_prefix = if trimmed_prefix.is_empty() {
            String::new()
        } else {
            format!("{trimmed_prefix}/")
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("open-ci-cache/0.1")
            .build()
            .map_err(|err| {
                CacheError::InvalidConfig(format!("failed building http client: {err}"))
            })?;

        Ok(Self {
            config,
            host,
            base_url,
            normalized_prefix,
            client,
            clock,
        })
    }

    /// Issues a SigV4-signed `PUT` object request.
    pub async fn put_object(&self, relative_key: &str, body: Bytes) -> Result<(), CacheError> {
        let canonical_uri = self.canonical_uri(relative_key);
        let url = format!("{}{canonical_uri}", self.base_url);
        let signed = self.sign_request("PUT", &canonical_uri, &body)?;

        let mut req = self
            .client
            .request(Method::PUT, &url)
            .header("host", &self.host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization)
            .header("content-type", "application/octet-stream")
            .body(body);
        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| CacheError::Storage(format!("s3 PUT failed: {err}")))?;
        if !resp.status().is_success() {
            return Err(CacheError::Storage(format!(
                "s3 PUT rejected with HTTP {}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }

    /// Issues a SigV4-signed `GET` object request. Returns `Ok(None)` on `404 Not Found`.
    pub async fn get_object(&self, relative_key: &str) -> Result<Option<Bytes>, CacheError> {
        let canonical_uri = self.canonical_uri(relative_key);
        let url = format!("{}{canonical_uri}", self.base_url);
        let signed = self.sign_request("GET", &canonical_uri, &[])?;

        let mut req = self
            .client
            .request(Method::GET, &url)
            .header("host", &self.host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization);
        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| CacheError::Storage(format!("s3 GET failed: {err}")))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(CacheError::Storage(format!(
                "s3 GET rejected with HTTP {}",
                resp.status().as_u16()
            )));
        }
        let data = resp
            .bytes()
            .await
            .map_err(|err| CacheError::Storage(format!("failed reading s3 GET body: {err}")))?;
        Ok(Some(data))
    }

    /// Issues a SigV4-signed `HEAD` object request. Returns `Ok(None)` on `404 Not Found`.
    pub async fn head_object(&self, relative_key: &str) -> Result<Option<u64>, CacheError> {
        let canonical_uri = self.canonical_uri(relative_key);
        let url = format!("{}{canonical_uri}", self.base_url);
        let signed = self.sign_request("HEAD", &canonical_uri, &[])?;

        let mut req = self
            .client
            .request(Method::HEAD, &url)
            .header("host", &self.host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization);
        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| CacheError::Storage(format!("s3 HEAD failed: {err}")))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(CacheError::Storage(format!(
                "s3 HEAD rejected with HTTP {}",
                resp.status().as_u16()
            )));
        }
        let len = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Some(len))
    }

    /// Issues a SigV4-signed `DELETE` object request.
    pub async fn delete_object(&self, relative_key: &str) -> Result<(), CacheError> {
        let canonical_uri = self.canonical_uri(relative_key);
        let url = format!("{}{canonical_uri}", self.base_url);
        let signed = self.sign_request("DELETE", &canonical_uri, &[])?;

        let mut req = self
            .client
            .request(Method::DELETE, &url)
            .header("host", &self.host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_sha256)
            .header("authorization", &signed.authorization);
        if let Some(token) = &signed.security_token {
            req = req.header("x-amz-security-token", token);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| CacheError::Storage(format!("s3 DELETE failed: {err}")))?;
        if resp.status() == StatusCode::NOT_FOUND || resp.status().is_success() {
            return Ok(());
        }
        Err(CacheError::Storage(format!(
            "s3 DELETE rejected with HTTP {}",
            resp.status().as_u16()
        )))
    }

    fn blob_key(blob_id: &str) -> String {
        format!("blobs/{blob_id}")
    }

    fn staged_block_key(blob_id: &str, block_id: &str) -> String {
        format!("staging/{blob_id}/{}", hash_block_id(block_id))
    }

    fn canonical_uri(&self, relative_key: &str) -> String {
        let full_key = format!(
            "{}{}",
            self.normalized_prefix,
            relative_key.trim_start_matches('/')
        );
        let encoded_bucket = encode_uri_segment(&self.config.bucket);
        let encoded_key = full_key
            .split('/')
            .map(encode_uri_segment)
            .collect::<Vec<_>>()
            .join("/");
        format!("/{encoded_bucket}/{encoded_key}")
    }

    fn sign_request(
        &self,
        method: &str,
        canonical_uri: &str,
        body: &[u8],
    ) -> Result<SignedS3Headers, CacheError> {
        let now_unix = self.clock.now_unix();
        let ts = i64::try_from(now_unix)
            .map_err(|_| CacheError::Internal("unix timestamp overflow".to_owned()))?;
        let dt = OffsetDateTime::from_unix_timestamp(ts)
            .map_err(|_| CacheError::Internal("invalid unix timestamp".to_owned()))?;

        let date_stamp = format!("{:04}{:02}{:02}", dt.year(), u8::from(dt.month()), dt.day());
        let amz_date = format!(
            "{date_stamp}T{:02}{:02}{:02}Z",
            dt.hour(),
            dt.minute(),
            dt.second()
        );
        let payload_sha256 = sha256_hex(body);
        let host_lower = self.host.to_ascii_lowercase();

        let (canonical_headers, signed_headers, security_token) = if let Some(token) =
            &self.config.session_token
        {
            let exposed = token.expose().to_owned();
            (
                format!(
                    "host:{host_lower}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\nx-amz-security-token:{exposed}\n"
                ),
                "host;x-amz-content-sha256;x-amz-date;x-amz-security-token",
                Some(exposed),
            )
        } else {
            (
                format!(
                    "host:{host_lower}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\n"
                ),
                "host;x-amz-content-sha256;x-amz-date",
                None,
            )
        };

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
        );
        let credential_scope = format!(
            "{date_stamp}/{}/{S3_SERVICE}/aws4_request",
            self.config.region
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );

        let k_date = hmac_sha256(
            format!("AWS4{}", self.config.secret_access_key.expose()).as_bytes(),
            date_stamp.as_bytes(),
        )?;
        let k_region = hmac_sha256(&k_date, self.config.region.as_bytes())?;
        let k_service = hmac_sha256(&k_region, S3_SERVICE.as_bytes())?;
        let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes())?);

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key_id
        );

        Ok(SignedS3Headers {
            amz_date,
            payload_sha256,
            authorization,
            security_token,
        })
    }
}

#[async_trait]
impl CacheStorage for S3CompatibleStorage {
    async fn put_blob(&self, blob_id: &str, data: Bytes) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        let size = data.len() as u64;
        self.put_object(&Self::blob_key(blob_id), data).await?;
        Ok(size)
    }

    async fn stage_block(
        &self,
        blob_id: &str,
        block_id: &str,
        data: Bytes,
    ) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        validate_block_id(block_id)?;
        let size = data.len() as u64;
        self.put_object(&Self::staged_block_key(blob_id, block_id), data)
            .await?;
        Ok(size)
    }

    async fn commit_blocks(
        &self,
        blob_id: &str,
        block_ids: &[String],
        max_entry_bytes: u64,
    ) -> Result<u64, CacheError> {
        validate_blob_id(blob_id)?;
        if block_ids.is_empty() {
            return Err(CacheError::BadRequest(
                "block list must not be empty".to_owned(),
            ));
        }

        let mut assembled = BytesMut::new();
        for block_id in block_ids {
            validate_block_id(block_id)?;
            let key = Self::staged_block_key(blob_id, block_id);
            let Some(chunk) = self.get_object(&key).await? else {
                return Err(CacheError::BadRequest(format!(
                    "staged block not found: {block_id}"
                )));
            };
            let next_len = (assembled.len() as u64).saturating_add(chunk.len() as u64);
            if next_len > max_entry_bytes {
                return Err(CacheError::PayloadTooLarge(format!(
                    "assembled blob exceeds max_entry_bytes ({max_entry_bytes})"
                )));
            }
            assembled.extend_from_slice(&chunk);
        }

        let total = assembled.len() as u64;
        self.put_object(&Self::blob_key(blob_id), assembled.freeze())
            .await?;
        for block_id in block_ids {
            let _ = self
                .delete_object(&Self::staged_block_key(blob_id, block_id))
                .await;
        }
        Ok(total)
    }

    async fn get_blob(&self, blob_id: &str) -> Result<Option<BlobPayload>, CacheError> {
        validate_blob_id(blob_id)?;
        Ok(self
            .get_object(&Self::blob_key(blob_id))
            .await?
            .map(BlobPayload::Bytes))
    }

    async fn head_blob(&self, blob_id: &str) -> Result<Option<u64>, CacheError> {
        validate_blob_id(blob_id)?;
        self.head_object(&Self::blob_key(blob_id)).await
    }

    async fn delete_blob(&self, blob_id: &str) -> Result<(), CacheError> {
        validate_blob_id(blob_id)?;
        self.delete_object(&Self::blob_key(blob_id)).await
    }

    async fn load_index(&self) -> Result<Vec<CacheEntry>, CacheError> {
        let Some(bytes) = self.get_object("index.jsonl").await? else {
            return Ok(Vec::new());
        };
        let raw = String::from_utf8_lossy(&bytes);
        Ok(decode_jsonl(&raw))
    }

    async fn save_index(&self, entries: &[CacheEntry]) -> Result<(), CacheError> {
        let bytes = encode_jsonl(entries)
            .map_err(|err| CacheError::Storage(format!("failed serializing cache index: {err}")))?;
        self.put_object("index.jsonl", Bytes::from(bytes)).await
    }
}

fn extract_host(base_url: &str) -> Result<String, CacheError> {
    let without_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .ok_or_else(|| CacheError::InvalidConfig("invalid s3 endpoint scheme".to_owned()))?;
    let host = without_scheme
        .split('/')
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| CacheError::InvalidConfig("missing s3 endpoint host".to_owned()))?;
    Ok(host.to_owned())
}

fn encode_uri_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&hex::encode_upper([byte]));
        }
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, CacheError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| CacheError::Internal("invalid hmac key".to_owned()))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}
