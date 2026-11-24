//! Local filesystem / `NVMe` blob store with atomic `0o600` writes.

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::{BlobStore, BlobUpload, ByteStream};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::io;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const READ_CHUNK_BYTES: usize = 256 * 1024;

/// Blob store rooted at a local directory: `blobs/<id>`, `staging/<id>/<block>`, `tmp/`.
#[derive(Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = root.into();
        remove_dir_if_exists(&root.join("tmp")).await?;
        for dir in ["blobs", "staging", "tmp"] {
            fs::create_dir_all(root.join(dir))
                .await
                .map_err(|err| storage_error(&format!("creating {dir} directory"), &err))?;
        }
        Ok(Self { root })
    }

    fn blob_path(&self, blob: &BlobId) -> PathBuf {
        self.root.join("blobs").join(blob.as_str())
    }

    fn staging_dir(&self, blob: &BlobId) -> PathBuf {
        self.root.join("staging").join(blob.as_str())
    }

    fn block_path(&self, blob: &BlobId, block: &BlockId) -> PathBuf {
        self.staging_dir(blob).join(block.storage_name())
    }

    fn temp_file(&self) -> TempFile {
        TempFile {
            path: self
                .root
                .join("tmp")
                .join(format!("{}.tmp", Uuid::new_v4())),
            persisted: false,
        }
    }

    async fn write_atomically(&self, dest: &Path, upload: BlobUpload) -> Result<(), CacheError> {
        let temp = self.temp_file();
        write_stream(&temp.path, upload.into_stream()).await?;
        temp.persist(dest).await
    }

    async fn staged_sizes(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
    ) -> Result<Vec<(PathBuf, u64)>, CacheError> {
        let mut sizes = Vec::with_capacity(blocks.len());
        for block in blocks {
            let path = self.block_path(blob, block);
            let size = file_size(&path).await?.ok_or_else(|| {
                CacheError::BadRequest(format!("staged block not found: {}", block.as_str()))
            })?;
            sizes.push((path, size));
        }
        Ok(sizes)
    }
}

#[async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        self.write_atomically(&self.blob_path(blob), upload).await
    }

    async fn stage_block(
        &self,
        blob: &BlobId,
        block: &BlockId,
        upload: BlobUpload,
    ) -> Result<(), CacheError> {
        fs::create_dir_all(self.staging_dir(blob))
            .await
            .map_err(|err| storage_error("creating staging directory", &err))?;
        self.write_atomically(&self.block_path(blob, block), upload)
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
        let staged = self.staged_sizes(blob, blocks).await?;
        let total = staged
            .iter()
            .try_fold(0u64, |acc, (_, size)| acc.checked_add(*size))
            .filter(|total| *total <= max_size)
            .ok_or_else(|| {
                CacheError::PayloadTooLarge(format!("joined blob exceeds {max_size} bytes"))
            })?;

        let temp = self.temp_file();
        concatenate(&temp.path, &staged).await?;
        temp.persist(&self.blob_path(blob)).await?;
        remove_dir_if_exists(&self.staging_dir(blob)).await?;
        Ok(total)
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        file_size(&self.blob_path(blob)).await
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        let mut file = match File::open(self.blob_path(blob)).await {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(CacheError::NotFound("blob not found".to_owned()));
            }
            Err(err) => return Err(storage_error("opening blob", &err)),
        };
        file.seek(SeekFrom::Start(range.start()))
            .await
            .map_err(|err| storage_error("seeking blob", &err))?;
        Ok(ReaderStream::with_capacity(file.take(range.byte_count()), READ_CHUNK_BYTES).boxed())
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        remove_if_exists(&self.blob_path(blob)).await?;
        remove_dir_if_exists(&self.staging_dir(blob)).await
    }
}

/// Temporary file removed on drop unless persisted, so cancelled uploads leave nothing behind.
struct TempFile {
    path: PathBuf,
    persisted: bool,
}

impl TempFile {
    async fn persist(mut self, dest: &Path) -> Result<(), CacheError> {
        fs::rename(&self.path, dest)
            .await
            .map_err(|err| storage_error("committing blob", &err))?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.persisted
            && let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %err, "failed removing cache temp file");
        }
    }
}

async fn write_stream(path: &Path, mut body: ByteStream) -> Result<(), CacheError> {
    let mut file = create_private_file(path).await?;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| CacheError::BadRequest(err.to_string()))?;
        file.write_all(&chunk)
            .await
            .map_err(|err| storage_error("writing blob", &err))?;
    }
    file.sync_all()
        .await
        .map_err(|err| storage_error("syncing blob", &err))
}

async fn concatenate(dest: &Path, parts: &[(PathBuf, u64)]) -> Result<(), CacheError> {
    let mut out = create_private_file(dest).await?;
    for (path, _) in parts {
        let mut part = File::open(path)
            .await
            .map_err(|err| storage_error("opening staged block", &err))?;
        tokio::io::copy(&mut part, &mut out)
            .await
            .map_err(|err| storage_error("joining staged blocks", &err))?;
    }
    out.sync_all()
        .await
        .map_err(|err| storage_error("syncing joined blob", &err))
}

async fn create_private_file(path: &Path) -> Result<File, CacheError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .await
        .map_err(|err| storage_error("creating file", &err))
}

async fn file_size(path: &Path) -> Result<Option<u64>, CacheError> {
    match fs::metadata(path).await {
        Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(storage_error("reading file metadata", &err)),
    }
}

async fn remove_if_exists(path: &Path) -> Result<(), CacheError> {
    match fs::remove_file(path).await {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            Err(storage_error("removing file", &err))
        }
        _ => Ok(()),
    }
}

async fn remove_dir_if_exists(path: &Path) -> Result<(), CacheError> {
    match fs::remove_dir_all(path).await {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            Err(storage_error("removing directory", &err))
        }
        _ => Ok(()),
    }
}

fn storage_error(action: &str, err: &io::Error) -> CacheError {
    CacheError::Storage(format!("{action}: {err}"))
}
