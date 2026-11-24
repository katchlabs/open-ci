//! Native `SigV4` S3-compatible blob store (AWS S3, Cloudflare R2, `MinIO`) with streaming bodies.
//!
//! Objects live at `{prefix}blobs/{blob}` and `{prefix}staging/{blob}/{block}` in a path-style
//! bucket. Committing staged blocks builds the final object with a multipart upload, copying
//! large blocks server-side and streaming runs of small blocks into parts of at least 5 MiB.

mod client;
mod sigv4;
mod xml;

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::{BlobStore, BlobUpload, ByteStream};
use async_trait::async_trait;
use client::{ObjectHead, S3Client};
use futures_util::{StreamExt, TryStreamExt, stream};
use open_ci_provider_aws::AwsCredentialsSource;
use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::sync::Arc;

const MIB: u64 = 1024 * 1024;
/// S3 rejects single `PutObject` bodies above 5 GiB.
const MAX_PUT_BYTES: u64 = 5 * 1024 * MIB;
/// Azure `Put Block` maximum; keeps every multipart part under the 5 GiB S3 part limit.
const MAX_BLOCK_BYTES: u64 = 4000 * MIB;
const MIN_PART_BYTES: u64 = 5 * MIB;
const MAX_PART_BYTES: u64 = 5 * 1024 * MIB;
const MAX_PARTS: usize = 10_000;
const MAX_PREFIX_BYTES: usize = 512;
const HEAD_CONCURRENCY: usize = 16;
const PART_CONCURRENCY: usize = 8;
const DELETE_CONCURRENCY: usize = 16;

/// Location of the cache objects in an S3-compatible service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3StoreConfig {
    /// Service origin such as `https://s3.us-east-1.amazonaws.com` or a `MinIO`/R2 URL.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix; normalized to empty or ending in `/`.
    pub prefix: String,
}

/// [`BlobStore`] backed by an S3-compatible bucket using path-style addressing.
#[derive(Debug)]
pub struct S3BlobStore {
    client: Arc<S3Client>,
    prefix: String,
}

#[derive(Clone, Debug)]
struct StagedBlock {
    key: String,
    head: ObjectHead,
}

impl S3BlobStore {
    pub fn new(
        config: S3StoreConfig,
        credentials: Arc<dyn AwsCredentialsSource>,
        http: reqwest::Client,
    ) -> Result<Self, CacheError> {
        let (origin, host) = parse_endpoint(&config.endpoint)?;
        validate_region(&config.region)?;
        validate_bucket(&config.bucket)?;
        let prefix = normalize_prefix(&config.prefix)?;
        Ok(Self {
            client: Arc::new(S3Client {
                http,
                credentials,
                origin,
                host,
                region: config.region,
                bucket: config.bucket,
            }),
            prefix,
        })
    }

    fn blob_key(&self, blob: &BlobId) -> String {
        format!("{}blobs/{blob}", self.prefix)
    }

    fn staging_prefix(&self, blob: &BlobId) -> String {
        format!("{}staging/{blob}/", self.prefix)
    }

    fn block_key(&self, blob: &BlobId, block: &BlockId) -> String {
        format!("{}{}", self.staging_prefix(blob), block.storage_name())
    }

    async fn head_staged(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
    ) -> Result<Vec<StagedBlock>, CacheError> {
        let distinct: HashSet<String> = blocks
            .iter()
            .map(|block| self.block_key(blob, block))
            .collect();
        let heads: HashMap<String, Option<ObjectHead>> = stream::iter(distinct)
            .map(|key| async move {
                let head = self.client.head_object(&key).await?;
                Ok::<_, CacheError>((key, head))
            })
            .buffer_unordered(HEAD_CONCURRENCY)
            .try_collect()
            .await?;
        blocks
            .iter()
            .map(|block| {
                let key = self.block_key(blob, block);
                let head = heads.get(&key).cloned().flatten().ok_or_else(|| {
                    CacheError::BadRequest(format!("staged block not found: {}", block.as_str()))
                })?;
                Ok(StagedBlock { key, head })
            })
            .collect()
    }

    async fn join(&self, target: &str, blocks: &[StagedBlock]) -> Result<(), CacheError> {
        let sizes: Vec<u64> = blocks.iter().map(|block| block.head.size).collect();
        let plan = plan_parts(&sizes)?;
        let upload_id = self.client.create_multipart_upload(target).await?;
        let joined = self
            .upload_and_complete(target, &upload_id, blocks, plan)
            .await;
        if joined.is_err()
            && let Err(abort) = self.client.abort_multipart_upload(target, &upload_id).await
        {
            tracing::warn!(error = %abort, "failed to abort s3 multipart upload");
        }
        joined
    }

    async fn upload_and_complete(
        &self,
        target: &str,
        upload_id: &str,
        blocks: &[StagedBlock],
        plan: Vec<Range<usize>>,
    ) -> Result<(), CacheError> {
        let parts: Vec<(u32, String)> = stream::iter(plan.into_iter().zip(1u32..))
            .map(|(range, number)| async move {
                let etag = self
                    .upload_group(target, upload_id, number, &blocks[range])
                    .await?;
                Ok::<_, CacheError>((number, etag))
            })
            .buffered(PART_CONCURRENCY)
            .try_collect()
            .await?;
        self.client
            .complete_multipart_upload(target, upload_id, &parts)
            .await
    }

    async fn upload_group(
        &self,
        target: &str,
        upload_id: &str,
        number: u32,
        group: &[StagedBlock],
    ) -> Result<String, CacheError> {
        if let [single] = group {
            return self
                .client
                .upload_part_copy(target, upload_id, number, &single.key, &single.head.etag)
                .await;
        }
        let body = self.concatenated(group);
        self.client
            .upload_part(target, upload_id, number, body)
            .await
    }

    /// Streams staged blocks back-to-back, each pinned to the `ETag` seen when sizing the join.
    fn concatenated(&self, group: &[StagedBlock]) -> BlobUpload {
        let total = group.iter().map(|block| block.head.size).sum();
        let client = Arc::clone(&self.client);
        let body = stream::iter(group.to_vec())
            .then(move |block| {
                let client = Arc::clone(&client);
                async move {
                    client
                        .get_object(&block.key, None, Some(&block.head.etag))
                        .await
                        .map_err(|err| io::Error::other(err.to_string()))
                }
            })
            .try_flatten()
            .boxed();
        BlobUpload::new(total, body)
    }

    async fn delete_staging(&self, blob: &BlobId) -> Result<(), CacheError> {
        let prefix = self.staging_prefix(blob);
        let mut token = None;
        loop {
            let page = self.client.list_objects(&prefix, token.as_deref()).await?;
            stream::iter(page.keys.into_iter().map(Ok::<_, CacheError>))
                .try_for_each_concurrent(DELETE_CONCURRENCY, |key| async move {
                    self.client.delete_object(&key).await
                })
                .await?;
            match page.next_token {
                Some(next) => token = Some(next),
                None => return Ok(()),
            }
        }
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        ensure_at_most(upload.content_length(), MAX_PUT_BYTES)?;
        self.client.put_object(&self.blob_key(blob), upload).await
    }

    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError> {
        ensure_at_most(upload.content_length(), MAX_BLOCK_BYTES)?;
        self.client
            .put_object(&self.block_key(blob, block), upload)
            .await
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
        let mut staged = self.head_staged(blob, blocks).await?;
        let total = staged
            .iter()
            .try_fold(0u64, |acc, block| acc.checked_add(block.head.size))
            .filter(|total| *total <= max_size)
            .ok_or_else(|| {
                CacheError::PayloadTooLarge(format!("joined blob exceeds {max_size} bytes"))
            })?;

        let target = self.blob_key(blob);
        staged.retain(|block| block.head.size > 0);
        if staged.is_empty() {
            self.client.put_empty_object(&target).await?;
        } else {
            self.join(&target, &staged).await?;
        }
        if let Err(err) = self.delete_staging(blob).await {
            tracing::warn!(error = %err, "committed blob but failed to remove staged blocks");
        }
        Ok(total)
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        Ok(self
            .client
            .head_object(&self.blob_key(blob))
            .await?
            .map(|head| head.size))
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        self.client
            .get_object(&self.blob_key(blob), Some(range), None)
            .await
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        self.client.delete_object(&self.blob_key(blob)).await?;
        self.delete_staging(blob).await
    }
}

fn ensure_at_most(content_length: u64, limit: u64) -> Result<(), CacheError> {
    if content_length > limit {
        return Err(CacheError::PayloadTooLarge(format!(
            "s3 object body exceeds {limit} bytes"
        )));
    }
    Ok(())
}

/// Splits positive block sizes into consecutive multipart parts: each part closes as soon as it
/// reaches the 5 MiB S3 minimum, so a block that is at least 5 MiB on its own becomes a
/// single-block (server-side copy) part and runs of smaller blocks are merged.
fn plan_parts(sizes: &[u64]) -> Result<Vec<Range<usize>>, CacheError> {
    let too_large =
        || CacheError::PayloadTooLarge("staged blocks do not fit S3 multipart limits".to_owned());
    let mut parts = Vec::new();
    let mut start = 0;
    let mut part_bytes = 0u64;
    for (index, size) in sizes.iter().enumerate() {
        part_bytes = part_bytes
            .checked_add(*size)
            .filter(|bytes| *bytes <= MAX_PART_BYTES)
            .ok_or_else(too_large)?;
        if part_bytes >= MIN_PART_BYTES {
            let end = index + 1;
            parts.push(start..end);
            start = end;
            part_bytes = 0;
        }
    }
    if start < sizes.len() {
        parts.push(start..sizes.len());
    }
    if parts.len() > MAX_PARTS {
        return Err(too_large());
    }
    Ok(parts)
}

fn parse_endpoint(endpoint: &str) -> Result<(String, String), CacheError> {
    let invalid = |reason: &str| CacheError::InvalidConfig(format!("s3 endpoint {reason}"));
    let url = reqwest::Url::parse(endpoint.trim()).map_err(|_| invalid("is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("must use http or https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not embed credentials"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("must not contain a path, query, or fragment"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| invalid("must include a host"))?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    Ok((format!("{}://{host}", url.scheme()), host))
}

fn validate_region(region: &str) -> Result<(), CacheError> {
    let valid = !region.is_empty()
        && region.len() <= 32
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(CacheError::InvalidConfig("invalid s3 region".to_owned()))
    }
}

fn validate_bucket(bucket: &str) -> Result<(), CacheError> {
    let bytes = bucket.as_bytes();
    let edge_ok = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let valid = (3..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && edge_ok(bytes.first())
        && edge_ok(bytes.last());
    if valid {
        Ok(())
    } else {
        Err(CacheError::InvalidConfig(
            "invalid s3 bucket name".to_owned(),
        ))
    }
}

/// Empty, or `/`-terminated segments that survive URL path normalization unchanged.
fn normalize_prefix(prefix: &str) -> Result<String, CacheError> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let valid = trimmed.len() < MAX_PREFIX_BYTES
        && !trimmed.chars().any(char::is_control)
        && trimmed
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if valid {
        Ok(format!("{trimmed}/"))
    } else {
        Err(CacheError::InvalidConfig(
            "invalid s3 key prefix".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part_bytes(sizes: &[u64], part: &Range<usize>) -> u64 {
        sizes[part.clone()].iter().sum()
    }

    fn assert_valid_plan(sizes: &[u64]) -> Result<(), CacheError> {
        let plan = plan_parts(sizes)?;
        let mut next = 0;
        for (index, part) in plan.iter().enumerate() {
            assert_eq!(part.start, next, "parts must be consecutive: {sizes:?}");
            assert!(part.end > part.start, "parts must be non-empty: {sizes:?}");
            let bytes = part_bytes(sizes, part);
            assert!(bytes <= MAX_PART_BYTES, "part over 5 GiB: {sizes:?}");
            if index + 1 < plan.len() {
                assert!(
                    bytes >= MIN_PART_BYTES,
                    "non-last part under 5 MiB: {sizes:?}"
                );
            }
            if part.len() > 1 {
                let leading: u64 = sizes[part.start..part.end - 1].iter().sum();
                assert!(
                    leading < MIN_PART_BYTES,
                    "merged a part that was already full"
                );
            }
            next = part.end;
        }
        assert_eq!(next, sizes.len(), "parts must cover every block: {sizes:?}");
        assert!(plan.len() <= MAX_PARTS);
        Ok(())
    }

    #[test]
    fn plans_copy_parts_for_large_blocks_and_merges_small_runs() -> Result<(), CacheError> {
        let sizes = [6 * MIB, MIB, MIB, 7 * MIB + 3, 10];
        assert_eq!(plan_parts(&sizes)?, vec![0..1, 1..4, 4..5]);
        assert_eq!(plan_parts(&[10])?, vec![0..1]);
        assert_eq!(plan_parts(&[MIB, MIB])?, vec![0..2]);
        assert_eq!(
            plan_parts(&[5 * MIB, 5 * MIB, 5 * MIB])?,
            vec![0..1, 1..2, 2..3]
        );
        Ok(())
    }

    #[test]
    fn plans_satisfy_s3_part_rules_for_pseudo_random_block_sequences() -> Result<(), CacheError> {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        let choices = [
            1,
            4096,
            MIB,
            4 * MIB,
            5 * MIB - 1,
            5 * MIB,
            32 * MIB,
            MAX_BLOCK_BYTES,
        ];
        for _ in 0..2000 {
            let count = usize::try_from(next() % 40 + 1).unwrap_or(1);
            let sizes: Vec<u64> = (0..count)
                .map(|_| {
                    let pick = usize::try_from(next()).unwrap_or(0) % choices.len();
                    choices[pick].saturating_sub(next() % 3).max(1)
                })
                .collect();
            assert_valid_plan(&sizes)?;
        }
        Ok(())
    }

    #[test]
    fn rejects_joins_that_need_more_than_ten_thousand_parts() {
        let sizes = vec![MIN_PART_BYTES; MAX_PARTS + 1];
        assert!(matches!(
            plan_parts(&sizes),
            Err(CacheError::PayloadTooLarge(_))
        ));
        assert!(plan_parts(&sizes[..MAX_PARTS]).is_ok());
    }

    #[test]
    fn rejects_parts_over_five_gib() {
        assert!(matches!(
            plan_parts(&[MIB, MAX_PART_BYTES]),
            Err(CacheError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn normalizes_prefix_and_validates_config() -> Result<(), CacheError> {
        assert_eq!(normalize_prefix("")?, "");
        assert_eq!(normalize_prefix("/")?, "");
        assert_eq!(normalize_prefix("/team/cache/")?, "team/cache/");
        assert_eq!(normalize_prefix("ci cache+v1")?, "ci cache+v1/");
        for bad in ["a//b", "a/../b", "./a", "a\nb"] {
            assert!(normalize_prefix(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_endpoint("https://s3.us-east-1.amazonaws.com")?,
            (
                "https://s3.us-east-1.amazonaws.com".to_owned(),
                "s3.us-east-1.amazonaws.com".to_owned()
            )
        );
        assert_eq!(
            parse_endpoint("http://127.0.0.1:9000/")?.1,
            "127.0.0.1:9000"
        );
        for bad in [
            "ftp://host",
            "https://user:pw@host",
            "https://host/bucket",
            "https://host/?x=1",
            "not a url",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
        assert!(validate_bucket("open-ci.cache-1").is_ok());
        for bad in ["ab", "Upper", "-lead", "trail-", "under_score"] {
            assert!(validate_bucket(bad).is_err(), "{bad}");
        }
        assert!(validate_region("auto").is_ok());
        assert!(validate_region("").is_err());
        assert!(validate_region("US East").is_err());
        Ok(())
    }
}
