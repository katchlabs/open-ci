//! Local filesystem / `NVMe` blob store with atomic `0o600` writes and zero-copy chunked manifests.

use crate::error::CacheError;
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::{BlobStore, BlobUpload, ByteStream};
use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const READ_CHUNK_BYTES: usize = 512 * 1024;
const WRITE_BUFFER_BYTES: usize = 512 * 1024;

/// Blob store rooted at a local directory: `blobs/<id>`, `committed/<id>/<block>`,
/// `staging/<id>/<block>`, `tmp/`.
#[derive(Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    total_size: u64,
    parts: Vec<ManifestPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestPart {
    file: String,
    size: u64,
}

struct OpenPartSlice {
    file: File,
    len: u64,
}

struct MultiPartReader {
    slices: VecDeque<OpenPartSlice>,
    current: Option<(File, u64)>,
    buf: BytesMut,
}

impl LocalBlobStore {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = root.into();
        remove_dir_if_exists(&root.join("tmp")).await?;
        for dir in ["blobs", "committed", "staging", "tmp"] {
            fs::create_dir_all(root.join(dir))
                .await
                .map_err(|err| storage_error(&format!("creating {dir} directory"), &err))?;
        }
        Ok(Self { root })
    }

    fn blob_path(&self, blob: &BlobId) -> PathBuf {
        self.root.join("blobs").join(blob.as_str())
    }

    fn manifest_path(&self, blob: &BlobId) -> PathBuf {
        self.root
            .join("blobs")
            .join(format!("{}.manifest", blob.as_str()))
    }

    fn committed_dir(&self, blob: &BlobId) -> PathBuf {
        self.root.join("committed").join(blob.as_str())
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

    async fn staged_parts(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
    ) -> Result<Vec<ManifestPart>, CacheError> {
        let staging_dir = self.staging_dir(blob);
        let mut parts = Vec::with_capacity(blocks.len());
        for block in blocks {
            let file = block.storage_name();
            let path = staging_dir.join(&file);
            let size = file_size(&path).await?.ok_or_else(|| {
                CacheError::BadRequest(format!("staged block not found: {}", block.as_str()))
            })?;
            parts.push(ManifestPart { file, size });
        }
        Ok(parts)
    }

    async fn read_manifest(&self, blob: &BlobId) -> Result<Option<Manifest>, CacheError> {
        match fs::read(self.manifest_path(blob)).await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(manifest) => Ok(Some(manifest)),
                Err(err) => {
                    tracing::warn!(blob = %blob, error = %err, "ignoring corrupt cache manifest");
                    Ok(None)
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(storage_error("reading manifest", &err)),
        }
    }

    async fn already_committed_size(
        &self,
        blob: &BlobId,
        blocks: &[BlockId],
        max_size: u64,
    ) -> Result<Option<u64>, CacheError> {
        if let Some(manifest) = self.read_manifest(blob).await? {
            let names_match = manifest.parts.len() == blocks.len()
                && manifest
                    .parts
                    .iter()
                    .zip(blocks)
                    .all(|(part, id)| part.file == id.storage_name());
            if names_match && manifest.total_size <= max_size {
                return Ok(Some(manifest.total_size));
            }
        }
        if blocks.len() == 1
            && let Some(size) = file_size(&self.blob_path(blob)).await?
            && size <= max_size
        {
            return Ok(Some(size));
        }
        Ok(None)
    }
}

#[async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, blob: &BlobId, upload: BlobUpload) -> Result<(), CacheError> {
        self.write_atomically(&self.blob_path(blob), upload).await?;
        remove_if_exists(&self.manifest_path(blob)).await?;
        remove_dir_if_exists(&self.committed_dir(blob)).await
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
        let parts = match self.staged_parts(blob, blocks).await {
            Ok(parts) => parts,
            Err(err) => {
                if let Some(total) = self.already_committed_size(blob, blocks, max_size).await? {
                    return Ok(total);
                }
                return Err(err);
            }
        };
        let total = parts
            .iter()
            .try_fold(0u64, |acc, part| acc.checked_add(part.size))
            .filter(|total| *total <= max_size)
            .ok_or_else(|| {
                CacheError::PayloadTooLarge(format!("joined blob exceeds {max_size} bytes"))
            })?;

        if let [only] = parts.as_slice() {
            let staged_file = self.staging_dir(blob).join(&only.file);
            fs::rename(&staged_file, self.blob_path(blob))
                .await
                .map_err(|err| storage_error("committing single block", &err))?;
            remove_if_exists(&self.manifest_path(blob)).await?;
            remove_dir_if_exists(&self.committed_dir(blob)).await?;
            remove_dir_if_exists(&self.staging_dir(blob)).await?;
            return Ok(total);
        }

        let committed = self.committed_dir(blob);
        remove_if_exists(&self.manifest_path(blob)).await?;
        remove_if_exists(&self.blob_path(blob)).await?;
        remove_dir_if_exists(&committed).await?;
        fs::rename(self.staging_dir(blob), &committed)
            .await
            .map_err(|err| storage_error("moving staged blocks", &err))?;
        let keep: HashSet<&str> = parts.iter().map(|part| part.file.as_str()).collect();
        prune_unreferenced_files(&committed, &keep).await?;

        let manifest_bytes = serde_json::to_vec(&Manifest {
            total_size: total,
            parts,
        })
        .map_err(|err| CacheError::Internal(format!("encoding manifest: {err}")))?;
        let temp = self.temp_file();
        write_bytes(&temp.path, &manifest_bytes).await?;
        temp.persist(&self.manifest_path(blob)).await?;
        Ok(total)
    }

    async fn size(&self, blob: &BlobId) -> Result<Option<u64>, CacheError> {
        if let Some(size) = file_size(&self.blob_path(blob)).await? {
            return Ok(Some(size));
        }
        Ok(self
            .read_manifest(blob)
            .await?
            .map(|manifest| manifest.total_size))
    }

    async fn read(&self, blob: &BlobId, range: ByteRange) -> Result<ByteStream, CacheError> {
        match File::open(self.blob_path(blob)).await {
            Ok(mut file) => {
                file.seek(SeekFrom::Start(range.start()))
                    .await
                    .map_err(|err| storage_error("seeking blob", &err))?;
                return Ok(ReaderStream::with_capacity(
                    file.take(range.byte_count()),
                    READ_CHUNK_BYTES,
                )
                .boxed());
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(storage_error("opening blob", &err)),
        }

        let manifest = self
            .read_manifest(blob)
            .await?
            .ok_or_else(|| CacheError::NotFound("blob not found".to_owned()))?;
        let slices = open_part_slices(&self.committed_dir(blob), &manifest.parts, range).await?;
        Ok(stream_part_slices(slices))
    }

    async fn delete(&self, blob: &BlobId) -> Result<(), CacheError> {
        remove_if_exists(&self.blob_path(blob)).await?;
        remove_if_exists(&self.manifest_path(blob)).await?;
        remove_dir_if_exists(&self.committed_dir(blob)).await?;
        remove_dir_if_exists(&self.staging_dir(blob)).await
    }
}

async fn open_part_slices(
    committed_dir: &Path,
    parts: &[ManifestPart],
    range: ByteRange,
) -> Result<VecDeque<OpenPartSlice>, CacheError> {
    let mut slices = VecDeque::new();
    let mut cursor = 0u64;
    for part in parts {
        if part.size == 0 {
            continue;
        }
        let part_start = cursor;
        let part_end_inclusive = cursor.saturating_add(part.size - 1);
        cursor = cursor.saturating_add(part.size);
        if part_end_inclusive < range.start() {
            continue;
        }
        if part_start > range.end_inclusive() {
            break;
        }
        let slice_start = range.start().max(part_start);
        let slice_end_inclusive = range.end_inclusive().min(part_end_inclusive);
        let offset = slice_start - part_start;
        let len = (slice_end_inclusive - slice_start) + 1;
        let mut file = File::open(committed_dir.join(&part.file))
            .await
            .map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    CacheError::NotFound("committed cache block missing".to_owned())
                } else {
                    storage_error("opening committed block", &err)
                }
            })?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .await
                .map_err(|err| storage_error("seeking committed block", &err))?;
        }
        slices.push_back(OpenPartSlice { file, len });
    }
    Ok(slices)
}

fn stream_part_slices(slices: VecDeque<OpenPartSlice>) -> ByteStream {
    let state = MultiPartReader {
        slices,
        current: None,
        buf: BytesMut::new(),
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some((file, remaining)) = state.current.as_mut() {
                if *remaining == 0 {
                    state.current = None;
                    continue;
                }
                let to_read = usize::try_from((*remaining).min(READ_CHUNK_BYTES as u64))
                    .unwrap_or(READ_CHUNK_BYTES);
                if state.buf.capacity() < to_read {
                    state.buf = BytesMut::with_capacity(to_read);
                }
                let limit = to_read as u64;
                let read = file.take(limit).read_buf(&mut state.buf).await?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "committed cache block truncated",
                    ));
                }
                *remaining -= read as u64;
                let chunk = state.buf.split().freeze();
                return Ok(Some((chunk, state)));
            }
            let Some(next) = state.slices.pop_front() else {
                return Ok(None);
            };
            state.current = Some((next.file, next.len));
        }
    })
    .boxed()
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
    let file = create_private_file(path).await?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| CacheError::BadRequest(err.to_string()))?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|err| storage_error("writing blob", &err))?;
    }
    writer
        .flush()
        .await
        .map_err(|err| storage_error("flushing blob", &err))
}

/// Writes a small file for atomic `rename` publication without `fsync`: on ext4 `data=ordered`
/// one fsync commits the whole journal transaction, stalling every concurrent upload until all
/// of their dirty pages reach disk. A manifest torn by power loss parses as a cache miss.
async fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let mut file = create_private_file(path).await?;
    file.write_all(bytes)
        .await
        .map_err(|err| storage_error("writing manifest", &err))?;
    file.flush()
        .await
        .map_err(|err| storage_error("flushing manifest", &err))
}

async fn prune_unreferenced_files(dir: &Path, keep: &HashSet<&str>) -> Result<(), CacheError> {
    let mut entries = fs::read_dir(dir)
        .await
        .map_err(|err| storage_error("listing committed blocks", &err))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|err| storage_error("reading committed directory", &err))?
    {
        let kept = entry
            .file_name()
            .to_str()
            .is_some_and(|name| keep.contains(name));
        if !kept {
            remove_if_exists(&entry.path()).await?;
        }
    }
    Ok(())
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
