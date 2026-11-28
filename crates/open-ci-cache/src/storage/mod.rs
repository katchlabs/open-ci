//! Streaming blob storage abstraction and its native backends.
//!
//! A [`BlobStore`] holds opaque cache archives and Azure-style staged blocks. It never sees cache
//! keys, scopes, or metadata: those live in [`crate::index::CacheIndex`].

pub mod gcs;
pub mod local;
pub mod memory;
pub mod s3;

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use std::io;

pub use gcs::{GcsBlobStore, GcsStoreConfig};
pub use local::LocalBlobStore;
pub use memory::{HotMemoryBlobStore, HotMemoryUsage};
pub use s3::{S3BlobStore, S3StoreConfig};

/// Fallible stream of body bytes moving between HTTP and a backend without full buffering.
pub type ByteStream = BoxStream<'static, io::Result<Bytes>>;

/// Request body with a declared length, enforced as the bytes flow.
#[derive(Debug)]
pub struct BlobUpload {
    content_length: u64,
    body: SizedStream,
}

struct SizedStream(ByteStream);

impl std::fmt::Debug for SizedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ByteStream")
    }
}

impl BlobUpload {
    /// Wraps `body`, failing the stream if it yields more or fewer than `content_length` bytes.
    pub fn new(content_length: u64, body: ByteStream) -> Self {
        Self {
            content_length,
            body: SizedStream(exact_length(body, content_length)),
        }
    }

    pub fn from_bytes(data: Bytes) -> Self {
        let len = data.len() as u64;
        Self::new(len, futures_util::stream::once(async { Ok(data) }).boxed())
    }

    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    pub fn into_stream(self) -> ByteStream {
        self.body.0
    }

    pub(crate) fn map_stream(self, f: impl FnOnce(ByteStream) -> ByteStream) -> Self {
        Self {
            content_length: self.content_length,
            body: SizedStream(f(self.body.0)),
        }
    }
}

/// Yields `body` only if it is exactly `expected` bytes. The chunk that completes the length is
/// held until the source is confirmed finished, so a remote store never receives a complete
/// declared length from a body that would turn out to be too long.
fn exact_length(body: ByteStream, expected: u64) -> ByteStream {
    futures_util::stream::unfold(Some((body, 0u64)), move |state| async move {
        let (mut body, seen) = state?;
        let item = match body.next().await {
            None if seen == expected => return None,
            None => Err(length_mismatch(expected)),
            Some(Err(err)) => Err(err),
            Some(Ok(chunk)) => {
                let seen = seen.saturating_add(chunk.len() as u64);
                if seen < expected {
                    return Some((Ok(chunk), Some((body, seen))));
                }
                if seen == expected && source_finished(&mut body).await {
                    return Some((Ok(chunk), None));
                }
                Err(length_mismatch(expected))
            }
        };
        Some((item, None))
    })
    .boxed()
}

async fn source_finished(body: &mut ByteStream) -> bool {
    while let Some(next) = body.next().await {
        match next {
            Ok(chunk) if chunk.is_empty() => {}
            _ => return false,
        }
    }
    true
}

fn length_mismatch(expected: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("request body did not match declared content length {expected}"),
    )
}

/// Native object storage for cache archives.
///
/// Implementations must be safe for concurrent calls on distinct blob IDs and must make a
/// committed blob visible atomically: readers observe either nothing or the complete content.
#[async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Stores a complete blob from one request body, replacing any existing content.
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError>;

    /// Stages one block of a multi-block upload, replacing a previous block with the same ID.
    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError>;

    /// Joins staged blocks in `blocks` order into the final blob and returns its size.
    ///
    /// Fails with [`CacheError::BadRequest`] when a block was never staged and with
    /// [`CacheError::PayloadTooLarge`] when the joined size would exceed `max_size`; in both cases
    /// no blob becomes visible. Staged blocks are removed after a successful commit.
    async fn commit_blocks(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
        max_size: u64,
    ) -> Result<u64, CacheError>;

    /// Size of a committed blob, or `None` when it does not exist.
    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError>;

    /// Streams `range` of a committed blob; [`CacheError::NotFound`] when it does not exist.
    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError>;

    /// Removes a committed blob and any staged blocks; deleting a missing blob succeeds.
    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError>;
}
