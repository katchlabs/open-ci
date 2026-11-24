//! Google Cloud Storage blob store over the native JSON API.
//!
//! Blobs live at `{prefix}blobs/{blob}` and staged blocks at `{prefix}staging/{blob}/{block}`.
//! Block commits join objects server-side with the compose API, so no bytes pass through the
//! cache host, and the final compose publishes the blob atomically.

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::{BlobStore, BlobUpload, ByteStream};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use open_ci_provider_gcp::GcpTokenSource;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, RANGE};
use reqwest::{Body, Client, Method, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

pub use open_ci_core::config::DEFAULT_GCS_ENDPOINT;

const MAX_COMPOSE_SOURCES: usize = 32;
const PARALLEL_REQUESTS: usize = 16;
const MAX_PREFIX_LEN: usize = 512;
const OCTET_STREAM: &str = "application/octet-stream";

/// Location of the cache objects inside one GCS bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcsStoreConfig {
    /// JSON API base URL, normally [`DEFAULT_GCS_ENDPOINT`]; override it to target an emulator.
    pub endpoint: String,
    pub bucket: String,
    /// Object name prefix, normalized to empty or ending in `/`.
    pub prefix: String,
}

impl GcsStoreConfig {
    /// Configuration for `bucket` on the production endpoint.
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            endpoint: DEFAULT_GCS_ENDPOINT.to_owned(),
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }
}

/// Blob store backed by Cloud Storage, authenticated by a [`GcpTokenSource`] bearer token.
#[derive(Debug)]
pub struct GcsBlobStore {
    client: Client,
    tokens: Arc<dyn GcpTokenSource>,
    endpoint: String,
    bucket: String,
    prefix: String,
}

#[derive(Debug, Deserialize)]
struct ObjectResource {
    size: String,
    generation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectList {
    #[serde(default)]
    items: Vec<ListedObject>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListedObject {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposeRequest<'a> {
    source_objects: Vec<ComposeSource<'a>>,
    destination: ComposeDestination,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposeSource<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_preconditions: Option<GenerationMatch>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationMatch {
    if_generation_match: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposeDestination {
    content_type: &'static str,
}

#[derive(Clone, Debug)]
struct Source {
    name: String,
    generation: Option<u64>,
}

#[derive(Debug)]
enum ComposeStep {
    Carry(Source),
    Join(String, Vec<Source>),
}

#[derive(Debug)]
struct ObjectInfo {
    size: u64,
    generation: u64,
}

impl GcsBlobStore {
    pub fn new(
        config: GcsStoreConfig,
        tokens: Arc<dyn GcpTokenSource>,
        client: Client,
    ) -> Result<Self, CacheError> {
        Ok(Self {
            client,
            tokens,
            endpoint: normalize_endpoint(&config.endpoint)?,
            bucket: validate_bucket(config.bucket)?,
            prefix: normalize_prefix(&config.prefix)?,
        })
    }

    fn blob_name(&self, blob: &BlobId) -> String {
        format!("{}blobs/{blob}", self.prefix)
    }

    fn staging_prefix(&self, blob: &BlobId) -> String {
        format!("{}staging/{blob}/", self.prefix)
    }

    fn block_name(&self, blob: &BlobId, block: &BlockId) -> String {
        format!("{}{}", self.staging_prefix(blob), block.storage_name())
    }

    fn bucket_url(&self) -> String {
        format!(
            "{}/storage/v1/b/{}/o",
            self.endpoint,
            encode_component(&self.bucket)
        )
    }

    fn object_url(&self, name: &str) -> String {
        format!("{}/{}", self.bucket_url(), encode_component(name))
    }

    async fn request(&self, method: Method, url: String) -> Result<RequestBuilder, CacheError> {
        let token =
            self.tokens.bearer_token().await.map_err(|err| {
                CacheError::Storage(format!("gcs access token unavailable: {err}"))
            })?;
        Ok(self.client.request(method, url).bearer_auth(token.expose()))
    }

    async fn send(
        &self,
        method: Method,
        url: String,
        action: &str,
        build: impl FnOnce(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response, CacheError> {
        let request = build(self.request(method.clone(), url).await?);
        request
            .send()
            .await
            .map_err(|err| transport_error(&method, action, &err))
    }

    async fn upload(&self, name: &str, upload: BlobUpload) -> Result<(), CacheError> {
        let length = upload.content_length();
        let body_failure = Arc::new(OnceLock::<String>::new());
        let observed = Arc::clone(&body_failure);
        let body = upload.into_stream().inspect_err(move |err| {
            observed.get_or_init(|| err.to_string());
        });
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.endpoint,
            encode_component(&self.bucket),
            encode_component(name)
        );
        let sent = self
            .request(Method::POST, url)
            .await?
            .header(CONTENT_TYPE, OCTET_STREAM)
            .header(CONTENT_LENGTH, length)
            .body(Body::wrap_stream(body))
            .send()
            .await;
        if let Some(message) = body_failure.get() {
            return Err(CacheError::BadRequest(message.clone()));
        }
        let response = sent.map_err(|err| transport_error(&Method::POST, "upload", &err))?;
        let object = object_resource(response, &Method::POST, "upload").await?;
        if object.size == length {
            Ok(())
        } else {
            Err(CacheError::Storage(format!(
                "gcs upload stored {} bytes, expected {length}",
                object.size
            )))
        }
    }

    async fn object_info(&self, name: &str) -> Result<Option<ObjectInfo>, CacheError> {
        let response = self
            .send(Method::GET, self.object_url(name), "object metadata", |r| r)
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        object_resource(response, &Method::GET, "object metadata")
            .await
            .map(Some)
    }

    async fn staged_sources(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
    ) -> Result<Vec<(Source, u64)>, CacheError> {
        let distinct: HashSet<BlockId> = blocks.iter().cloned().collect();
        let infos: HashMap<BlockId, ObjectInfo> = stream::iter(distinct)
            .map(|block| async move {
                let info = self
                    .object_info(&self.block_name(blob, &block))
                    .await?
                    .ok_or_else(|| {
                        CacheError::BadRequest(format!(
                            "staged block not found: {}",
                            block.as_str()
                        ))
                    })?;
                Ok::<_, CacheError>((block, info))
            })
            .buffer_unordered(PARALLEL_REQUESTS)
            .try_collect()
            .await?;
        blocks
            .iter()
            .map(|block| {
                let info = infos.get(block).ok_or_else(|| {
                    CacheError::Internal("staged block lookup missing".to_owned())
                })?;
                let source = Source {
                    name: self.block_name(blob, block),
                    generation: Some(info.generation),
                };
                Ok((source, info.size))
            })
            .collect()
    }

    async fn compose(&self, destination: &str, sources: &[Source]) -> Result<(), CacheError> {
        let body = ComposeRequest {
            source_objects: sources
                .iter()
                .map(|source| ComposeSource {
                    name: &source.name,
                    object_preconditions: source.generation.map(|if_generation_match| {
                        GenerationMatch {
                            if_generation_match,
                        }
                    }),
                })
                .collect(),
            destination: ComposeDestination {
                content_type: OCTET_STREAM,
            },
        };
        let url = format!("{}/compose", self.object_url(destination));
        let response = self
            .send(Method::POST, url, "compose", |r| r.json(&body))
            .await?;
        object_resource(response, &Method::POST, "compose")
            .await
            .map(drop)
    }

    /// Folds levels of more than 32 sources into intermediates under the staging prefix, then
    /// composes the last level into `destination`. Intermediate names land in `created` before
    /// they are written so a failed commit can remove them.
    async fn compose_tree(
        &self,
        blob: &BlobId,
        destination: &str,
        mut level: Vec<Source>,
        created: &mut Vec<String>,
    ) -> Result<(), CacheError> {
        let run = Uuid::new_v4().simple();
        let staging = self.staging_prefix(blob);
        let mut depth = 0u32;
        while level.len() > MAX_COMPOSE_SOURCES {
            let groups: Vec<ComposeStep> = level
                .chunks(MAX_COMPOSE_SOURCES)
                .enumerate()
                .map(|(index, group)| match group {
                    [single] => ComposeStep::Carry(single.clone()),
                    _ => ComposeStep::Join(
                        format!("{staging}compose-{run}-{depth}-{index}"),
                        group.to_vec(),
                    ),
                })
                .collect();
            created.extend(groups.iter().filter_map(|step| match step {
                ComposeStep::Join(name, _) => Some(name.clone()),
                ComposeStep::Carry(_) => None,
            }));
            level = stream::iter(groups)
                .map(|step| async move {
                    match step {
                        ComposeStep::Carry(source) => Ok::<_, CacheError>(source),
                        ComposeStep::Join(name, group) => {
                            self.compose(&name, &group).await?;
                            Ok(Source {
                                name,
                                generation: None,
                            })
                        }
                    }
                })
                .buffered(PARALLEL_REQUESTS)
                .try_collect()
                .await?;
            depth += 1;
        }
        self.compose(destination, &level).await
    }

    async fn delete_object(&self, name: &str) -> Result<(), CacheError> {
        let response = self
            .send(Method::DELETE, self.object_url(name), "delete", |r| r)
            .await?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_error(&Method::DELETE, "delete", response).await)
        }
    }

    async fn delete_objects(
        &self,
        names: impl IntoIterator<Item = String>,
    ) -> Result<(), CacheError> {
        let names: BTreeSet<String> = names.into_iter().collect();
        stream::iter(names)
            .map(|name| async move { self.delete_object(&name).await })
            .buffer_unordered(PARALLEL_REQUESTS)
            .try_collect()
            .await
    }

    async fn list_names(&self, prefix: &str) -> Result<Vec<String>, CacheError> {
        let mut names = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}?prefix={}", self.bucket_url(), encode_component(prefix));
            if let Some(token) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(&encode_component(token));
            }
            let response = self.send(Method::GET, url, "list", |r| r).await?;
            if !response.status().is_success() {
                return Err(status_error(&Method::GET, "list", response).await);
            }
            let page: ObjectList = response
                .json()
                .await
                .map_err(|err| transport_error(&Method::GET, "list", &err))?;
            names.extend(page.items.into_iter().map(|item| item.name));
            match page.next_page_token.filter(|token| !token.is_empty()) {
                Some(token) => page_token = Some(token),
                None => return Ok(names),
            }
        }
    }
}

#[async_trait]
impl BlobStore for GcsBlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        self.upload(&self.blob_name(blob), upload).await
    }

    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError> {
        self.upload(&self.block_name(blob, block), upload).await
    }

    async fn commit_blocks(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
        max_size: u64,
    ) -> Result<u64, CacheError> {
        if blocks.is_empty() {
            return Err(CacheError::BadRequest(
                "block list must not be empty".to_owned(),
            ));
        }
        let staged = self.staged_sources(blob, blocks).await?;
        let total = staged
            .iter()
            .try_fold(0u64, |acc, (_, size)| acc.checked_add(*size))
            .filter(|total| *total <= max_size)
            .ok_or_else(|| {
                CacheError::PayloadTooLarge(format!("joined blob exceeds {max_size} bytes"))
            })?;
        let sources: Vec<Source> = staged.into_iter().map(|(source, _)| source).collect();

        let mut intermediates = Vec::new();
        let composed = self
            .compose_tree(blob, &self.blob_name(blob), sources, &mut intermediates)
            .await;
        if let Err(err) = composed {
            if let Err(cleanup) = self.delete_objects(intermediates).await {
                tracing::warn!(error = %cleanup, "failed to remove gcs compose intermediates");
            }
            return Err(err);
        }
        let staging = self.staging_prefix(blob);
        let cleaned = match self.list_names(&staging).await {
            Ok(names) => self.delete_objects(names).await,
            Err(err) => Err(err),
        };
        if let Err(err) = cleaned {
            tracing::warn!(blob = %blob, error = %err, "gcs blob published; staged objects remain until it is deleted");
        }
        Ok(total)
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        Ok(self
            .object_info(&self.blob_name(blob))
            .await?
            .map(|info| info.size))
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        let url = format!("{}?alt=media", self.object_url(&self.blob_name(blob)));
        let response = self
            .send(Method::GET, url, "read", |r| {
                r.header(RANGE, range.to_header_value())
            })
            .await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(CacheError::NotFound("blob not found".to_owned()));
        }
        if !status.is_success() {
            return Err(status_error(&Method::GET, "read", response).await);
        }
        let ranged = status == StatusCode::PARTIAL_CONTENT
            || response.content_length() == Some(range.byte_count());
        if !ranged {
            return Err(CacheError::Storage(format!(
                "gcs GET read ignored range {}",
                range.to_header_value()
            )));
        }
        Ok(response.bytes_stream().map_err(io::Error::other).boxed())
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        self.delete_object(&self.blob_name(blob)).await?;
        let staged = self.list_names(&self.staging_prefix(blob)).await?;
        self.delete_objects(staged).await
    }
}

async fn object_resource(
    response: Response,
    method: &Method,
    action: &str,
) -> Result<ObjectInfo, CacheError> {
    if !response.status().is_success() {
        return Err(status_error(method, action, response).await);
    }
    let resource: ObjectResource = response
        .json()
        .await
        .map_err(|err| transport_error(method, action, &err))?;
    Ok(ObjectInfo {
        size: parse_int64(&resource.size, "size")?,
        generation: parse_int64(&resource.generation, "generation")?,
    })
}

fn parse_int64(raw: &str, field: &str) -> Result<u64, CacheError> {
    raw.parse()
        .map_err(|_| CacheError::Storage(format!("gcs returned a non-numeric object {field}")))
}

async fn status_error(method: &Method, action: &str, response: Response) -> CacheError {
    let status = response.status().as_u16();
    let message = match response.json::<ErrorEnvelope>().await {
        Ok(envelope) => format!(
            "gcs {method} {action} returned http {status}: {}",
            envelope.error.message
        ),
        Err(_) => format!("gcs {method} {action} returned http {status}"),
    };
    CacheError::Storage(message)
}

fn transport_error(method: &Method, action: &str, err: &reqwest::Error) -> CacheError {
    CacheError::Storage(format!("gcs {method} {action} failed: {err}"))
}

/// Percent-encodes everything outside RFC 3986 unreserved characters, so `/` becomes `%2F` and
/// a full object name fits in one path segment or query value.
fn encode_component(raw: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

fn normalize_endpoint(raw: &str) -> Result<String, CacheError> {
    let invalid = |msg: &str| CacheError::InvalidConfig(format!("gcs endpoint {msg}"));
    let url = Url::parse(raw.trim()).map_err(|_| invalid("is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("must use http or https"));
    }
    if url.host_str().is_none() {
        return Err(invalid("must include a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not embed credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("must not include a query or fragment"));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn validate_bucket(bucket: String) -> Result<String, CacheError> {
    let bytes = bucket.as_bytes();
    let valid = (3..=222).contains(&bytes.len())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(bucket)
    } else {
        Err(CacheError::InvalidConfig(format!(
            "gcs bucket name {bucket:?} is invalid"
        )))
    }
}

fn normalize_prefix(raw: &str) -> Result<String, CacheError> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let valid = trimmed.len() <= MAX_PREFIX_LEN
        && !trimmed.chars().any(char::is_control)
        && trimmed
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if valid {
        Ok(format!("{trimmed}/"))
    } else {
        Err(CacheError::InvalidConfig(format!(
            "gcs prefix {raw:?} is invalid"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_encode_as_one_path_segment() {
        assert_eq!(
            encode_component("ci/staging/a_b-1/ff~x y+%"),
            "ci%2Fstaging%2Fa_b-1%2Fff~x%20y%2B%25"
        );
    }

    #[test]
    fn prefix_normalizes_to_empty_or_trailing_slash() {
        let norm = |raw: &str| normalize_prefix(raw).ok();
        assert_eq!(norm(""), Some(String::new()));
        assert_eq!(norm("/"), Some(String::new()));
        assert_eq!(norm("cache"), Some("cache/".to_owned()));
        assert_eq!(norm("/org/cache/"), Some("org/cache/".to_owned()));
        for bad in ["a//b", "a/../b", "./a", "a\nb", &"x".repeat(513)] {
            assert!(norm(bad).is_none(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn config_rejects_unusable_endpoints_and_buckets() {
        assert_eq!(
            normalize_endpoint(" https://storage.googleapis.com/ ").ok(),
            Some(DEFAULT_GCS_ENDPOINT.to_owned())
        );
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:4443/gcs/").ok(),
            Some("http://127.0.0.1:4443/gcs".to_owned())
        );
        for bad in [
            "storage.googleapis.com",
            "ftp://host",
            "https://user:pw@host",
            "https://host/?x=1",
        ] {
            assert!(
                matches!(normalize_endpoint(bad), Err(CacheError::InvalidConfig(_))),
                "{bad}"
            );
        }
        assert!(validate_bucket("open-ci_cache.v1".to_owned()).is_ok());
        for bad in ["ab", "Upper", "-lead", "trail.", "has/slash"] {
            assert!(validate_bucket(bad.to_owned()).is_err(), "{bad}");
        }
    }
}
