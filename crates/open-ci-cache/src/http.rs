//! Axum HTTP server implementing the GitHub Actions Cache v2 Twirp API and Azure-compatible blob endpoints.

use crate::auth::{
    BlobOp, CacheTokenClaims, sign_blob_url, validate_block_id, validate_key, validate_version,
    verify_blob_signature, verify_cache_token,
};
use crate::config::{CacheBackendConfig, CacheConfig};
use crate::error::CacheError;
use crate::index::CacheIndex;
use crate::storage::{BlobPayload, CacheStorage, LocalFsStorage, S3CompatibleStorage};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use open_ci_core::{Clock, SystemClock};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Deserializer, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

const TWIRP_MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESTORE_KEYS: usize = 64;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Shared runtime state backing [`build_cache_router`] and [`CacheServer`].
#[derive(Debug)]
pub struct CacheState {
    pub config: CacheConfig,
    pub storage: Arc<dyn CacheStorage>,
    pub clock: Arc<dyn Clock>,
    index: RwLock<CacheIndex>,
}

impl CacheState {
    /// Constructs a [`CacheState`], validating configuration and hydrating the index from storage.
    pub async fn load(
        config: CacheConfig,
        storage: Arc<dyn CacheStorage>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, CacheError> {
        config.validate()?;
        let persisted = storage.load_index().await?;
        Ok(Self {
            config,
            storage,
            clock,
            index: RwLock::new(CacheIndex::from_entries(persisted)),
        })
    }
}

/// Standalone GitHub Actions v2 transparent cache server.
#[derive(Debug)]
pub struct CacheServer {
    listen_addr: String,
    state: Arc<CacheState>,
}

impl CacheServer {
    /// Initializes a [`CacheServer`] from [`CacheConfig`] using the configured storage backend.
    pub async fn new(config: CacheConfig) -> Result<Self, CacheError> {
        config.validate()?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let storage: Arc<dyn CacheStorage> = match &config.backend {
            CacheBackendConfig::LocalFs { root_dir } => Arc::new(
                LocalFsStorage::open_with_clock(
                    root_dir.clone(),
                    config.max_total_bytes,
                    Arc::clone(&clock),
                )
                .await?,
            ),
            CacheBackendConfig::S3(s3_cfg) => Arc::new(S3CompatibleStorage::new(
                s3_cfg.clone(),
                Arc::clone(&clock),
            )?),
        };
        Self::with_storage(config, storage, clock).await
    }

    /// Initializes a [`CacheServer`] with a custom [`CacheStorage`] implementation and [`Clock`].
    pub async fn with_storage(
        config: CacheConfig,
        storage: Arc<dyn CacheStorage>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, CacheError> {
        let listen_addr = config.listen_addr.clone();
        let state = Arc::new(CacheState::load(config, storage, clock).await?);
        Ok(Self { listen_addr, state })
    }

    /// Returns the Axum [`Router`] for embedding or in-process testing.
    pub fn router(&self) -> Router {
        build_cache_router(Arc::clone(&self.state))
    }

    /// Returns a handle to the shared [`CacheState`].
    pub fn state(&self) -> Arc<CacheState> {
        Arc::clone(&self.state)
    }

    /// Binds to the configured listen address and serves requests until terminated.
    pub async fn serve(self) -> Result<(), CacheError> {
        let listener = TcpListener::bind(&self.listen_addr).await.map_err(|err| {
            CacheError::Internal(format!("failed binding {}: {err}", self.listen_addr))
        })?;
        axum::serve(listener, self.router())
            .await
            .map_err(|err| CacheError::Internal(format!("cache server exited with error: {err}")))
    }

    /// Binds to the configured listen address and serves requests until `shutdown` resolves.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), CacheError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(&self.listen_addr).await.map_err(|err| {
            CacheError::Internal(format!("failed binding {}: {err}", self.listen_addr))
        })?;
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|err| CacheError::Internal(format!("cache server exited with error: {err}")))
    }
}

/// Builds the Axum HTTP [`Router`] exposing GitHub Actions Cache v2 Twirp RPCs and blob endpoints.
pub fn build_cache_router(state: Arc<CacheState>) -> Router {
    let twirp_routes = Router::new()
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry",
            post(create_cache_entry),
        )
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload",
            post(finalize_cache_entry_upload),
        )
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL",
            post(get_cache_entry_download_url),
        )
        .layer(DefaultBodyLimit::max(TWIRP_MAX_BODY_BYTES));

    let blob_routes = Router::new()
        .route(
            "/v1/cache/blobs/{id}",
            put(put_blob).get(get_blob).head(head_blob),
        )
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .route("/healthz", get(healthz))
        .merge(twirp_routes)
        .merge(blob_routes)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct CreateCacheEntryRequest {
    key: String,
    version: String,
}

#[derive(Serialize)]
struct CreateCacheEntryResponse {
    ok: bool,
    signed_upload_url: String,
}

async fn create_cache_entry(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<CreateCacheEntryRequest>,
) -> Result<Json<CreateCacheEntryResponse>, CacheError> {
    let claims = authenticate_bearer(&state, &headers)?;
    validate_key(&req.key)?;
    validate_version(&req.version)?;

    let now = state.clock.now_unix();
    let blob_id = Uuid::new_v4().to_string();
    let exp = now.saturating_add(state.config.signed_url_ttl_seconds);
    let signed_upload_url = sign_blob_url(
        &state.config.public_base_url,
        &state.config.signing_key,
        &blob_id,
        BlobOp::Put,
        exp,
    )?;

    state
        .index
        .write()
        .await
        .register_pending(&claims.scope, &req.key, &req.version, blob_id, now);

    Ok(Json(CreateCacheEntryResponse {
        ok: true,
        signed_upload_url,
    }))
}

#[derive(Deserialize)]
struct FinalizeCacheEntryUploadRequest {
    key: String,
    version: String,
    #[serde(
        default,
        alias = "sizeBytes",
        deserialize_with = "deserialize_optional_u64"
    )]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct FinalizeCacheEntryUploadResponse {
    ok: bool,
    entry_id: String,
}

async fn finalize_cache_entry_upload(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<FinalizeCacheEntryUploadRequest>,
) -> Result<Json<FinalizeCacheEntryUploadResponse>, CacheError> {
    let claims = authenticate_bearer(&state, &headers)?;
    validate_key(&req.key)?;
    validate_version(&req.version)?;

    if let Some(declared_size) = req.size_bytes
        && declared_size > state.config.max_entry_bytes
    {
        return Err(CacheError::PayloadTooLarge(format!(
            "declared size exceeds max_entry_bytes ({})",
            state.config.max_entry_bytes
        )));
    }

    let pending = state
        .index
        .write()
        .await
        .take_pending(&claims.scope, &req.key, &req.version)
        .ok_or_else(|| {
            CacheError::NotFound("no pending upload found for key and version".to_owned())
        })?;

    let Some(actual_size) = state.storage.head_blob(&pending.blob_id).await? else {
        return Err(CacheError::NotFound(
            "uploaded blob does not exist in storage".to_owned(),
        ));
    };
    if actual_size > state.config.max_entry_bytes {
        let _ = state.storage.delete_blob(&pending.blob_id).await;
        return Err(CacheError::PayloadTooLarge(format!(
            "uploaded blob exceeds max_entry_bytes ({})",
            state.config.max_entry_bytes
        )));
    }

    let now = state.clock.now_unix();
    let (entry, replaced_blob, snapshot) = {
        let mut index = state.index.write().await;
        let (entry, replaced) = index.commit_entry(
            &claims.scope,
            &req.key,
            &req.version,
            pending.blob_id,
            actual_size,
            now,
        );
        (entry, replaced, index.snapshot())
    };
    state.storage.save_index(&snapshot).await?;
    if let Some(old_blob_id) = replaced_blob {
        let _ = state.storage.delete_blob(&old_blob_id).await;
    }

    Ok(Json(FinalizeCacheEntryUploadResponse {
        ok: true,
        entry_id: entry.entry_id.to_string(),
    }))
}

#[derive(Deserialize)]
struct GetCacheEntryDownloadUrlRequest {
    key: String,
    #[serde(default, alias = "restoreKeys")]
    restore_keys: Vec<String>,
    version: String,
}

#[derive(Serialize)]
struct GetCacheEntryDownloadUrlResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_download_url: Option<String>,
}

async fn get_cache_entry_download_url(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<GetCacheEntryDownloadUrlRequest>,
) -> Result<Json<GetCacheEntryDownloadUrlResponse>, CacheError> {
    let claims = authenticate_bearer(&state, &headers)?;
    validate_key(&req.key)?;
    validate_version(&req.version)?;
    if req.restore_keys.len() > MAX_RESTORE_KEYS {
        return Err(CacheError::BadRequest(format!(
            "restore_keys exceeds maximum of {MAX_RESTORE_KEYS}"
        )));
    }
    for prefix in &req.restore_keys {
        validate_key(prefix)?;
    }

    let candidates = state.index.read().await.find_candidates(
        &claims.scope,
        &req.key,
        &req.restore_keys,
        &req.version,
    );

    let now = state.clock.now_unix();
    let mut pruned_any = false;
    for candidate in candidates {
        if state.storage.head_blob(&candidate.blob_id).await?.is_some() {
            let snapshot = {
                let mut index = state.index.write().await;
                index.touch(&candidate.scope, &candidate.key, &candidate.version, now);
                index.snapshot()
            };
            let _ = state.storage.save_index(&snapshot).await;

            let exp = now.saturating_add(state.config.signed_url_ttl_seconds);
            let signed_download_url = sign_blob_url(
                &state.config.public_base_url,
                &state.config.signing_key,
                &candidate.blob_id,
                BlobOp::Get,
                exp,
            )?;
            return Ok(Json(GetCacheEntryDownloadUrlResponse {
                ok: true,
                matched_key: Some(candidate.key),
                signed_download_url: Some(signed_download_url),
            }));
        }
        state
            .index
            .write()
            .await
            .remove(&candidate.scope, &candidate.key, &candidate.version);
        pruned_any = true;
    }

    if pruned_any {
        let snapshot = state.index.read().await.snapshot();
        let _ = state.storage.save_index(&snapshot).await;
    }

    Ok(Json(GetCacheEntryDownloadUrlResponse {
        ok: false,
        matched_key: None,
        signed_download_url: None,
    }))
}

#[derive(Deserialize)]
struct BlobQuery {
    exp: Option<u64>,
    op: Option<String>,
    sig: Option<String>,
    comp: Option<String>,
    blockid: Option<String>,
}

enum PutMode<'a> {
    SingleShot,
    StageBlock(&'a str),
    CommitBlockList,
}

async fn put_blob(
    State(state): State<Arc<CacheState>>,
    Path(blob_id): Path<String>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, CacheError> {
    verify_blob_signature(
        &state.config.signing_key,
        &blob_id,
        BlobOp::Put,
        query.op.as_deref(),
        query.exp,
        query.sig.as_deref(),
        state.clock.now_unix(),
    )?;

    let (mode, body_limit) = match query.comp.as_deref() {
        None => (PutMode::SingleShot, state.config.max_entry_bytes),
        Some("block") => {
            let block_id = query.blockid.as_deref().ok_or_else(|| {
                CacheError::BadRequest("comp=block requires blockid query parameter".to_owned())
            })?;
            validate_block_id(block_id)?;
            (PutMode::StageBlock(block_id), state.config.max_entry_bytes)
        }
        Some("blocklist") => (PutMode::CommitBlockList, TWIRP_MAX_BODY_BYTES as u64),
        Some(other) => {
            return Err(CacheError::BadRequest(format!(
                "unsupported comp query parameter: {other}"
            )));
        }
    };
    check_content_length_limit(&headers, body_limit)?;
    let bytes = read_bounded_body(body, body_limit).await?;

    match mode {
        PutMode::SingleShot => {
            state.storage.put_blob(&blob_id, bytes).await?;
        }
        PutMode::StageBlock(block_id) => {
            state.storage.stage_block(&blob_id, block_id, bytes).await?;
        }
        PutMode::CommitBlockList => {
            let block_ids = parse_block_list_xml(&bytes)?;
            state
                .storage
                .commit_blocks(&blob_id, &block_ids, state.config.max_entry_bytes)
                .await?;
        }
    }

    Ok((
        StatusCode::CREATED,
        [("x-ms-request-id", Uuid::new_v4().to_string())],
    )
        .into_response())
}

async fn head_blob(
    State(state): State<Arc<CacheState>>,
    Path(blob_id): Path<String>,
    Query(query): Query<BlobQuery>,
) -> Result<Response, CacheError> {
    verify_blob_signature(
        &state.config.signing_key,
        &blob_id,
        BlobOp::Get,
        query.op.as_deref(),
        query.exp,
        query.sig.as_deref(),
        state.clock.now_unix(),
    )?;

    let Some(size) = state.storage.head_blob(&blob_id).await? else {
        return Err(CacheError::NotFound("blob not found".to_owned()));
    };

    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(len) = HeaderValue::from_str(&size.to_string()) {
        headers.insert(CONTENT_LENGTH, len);
    }
    Ok(resp)
}

async fn get_blob(
    State(state): State<Arc<CacheState>>,
    Path(blob_id): Path<String>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Response, CacheError> {
    verify_blob_signature(
        &state.config.signing_key,
        &blob_id,
        BlobOp::Get,
        query.op.as_deref(),
        query.exp,
        query.sig.as_deref(),
        state.clock.now_unix(),
    )?;

    let Some(payload) = state.storage.get_blob(&blob_id).await? else {
        return Err(CacheError::NotFound("blob not found".to_owned()));
    };

    let total_size = payload.len();
    let byte_range = headers
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|raw| parse_single_byte_range(raw, total_size))
        .transpose()?;

    build_blob_response(payload, total_size, byte_range)
}

fn build_blob_response(
    payload: BlobPayload,
    total_size: u64,
    byte_range: Option<(u64, u64)>,
) -> Result<Response, CacheError> {
    let (status, start, end) = match byte_range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e),
        None if total_size == 0 => {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::OK;
            let hdrs = resp.headers_mut();
            hdrs.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            hdrs.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            hdrs.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
            return Ok(resp);
        }
        None => (StatusCode::OK, 0, total_size.saturating_sub(1)),
    };

    let content_len = end.saturating_sub(start).saturating_add(1);
    let body = match payload {
        BlobPayload::Bytes(data) => {
            let start_idx = usize::try_from(start)
                .map_err(|_| CacheError::RangeNotSatisfiable("range start overflow".to_owned()))?;
            let end_excl = usize::try_from(end.saturating_add(1))
                .map_err(|_| CacheError::RangeNotSatisfiable("range end overflow".to_owned()))?;
            Body::from(data.slice(start_idx..end_excl))
        }
        BlobPayload::File { path, .. } => stream_file_range(path, start, content_len),
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let hdrs = resp.headers_mut();
    hdrs.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    hdrs.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(val) = HeaderValue::from_str(&content_len.to_string()) {
        hdrs.insert(CONTENT_LENGTH, val);
    }
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(val) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total_size}"))
    {
        hdrs.insert(CONTENT_RANGE, val);
    }
    Ok(resp)
}

fn stream_file_range(path: PathBuf, start: u64, length: u64) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let mut file = match File::open(&path).await {
            Ok(f) => f,
            Err(err) => {
                let _ = tx.send(Err(err)).await;
                return;
            }
        };
        if start > 0
            && let Err(err) = file.seek(SeekFrom::Start(start)).await
        {
            let _ = tx.send(Err(err)).await;
            return;
        }
        let mut remaining = length;
        let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
        while remaining > 0 {
            let to_read = usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64))
                .unwrap_or(STREAM_CHUNK_BYTES);
            match file.read(&mut buf[..to_read]).await {
                Ok(0) => break,
                Ok(n) => {
                    remaining = remaining.saturating_sub(n as u64);
                    if tx
                        .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });
    Body::from_stream(ReceiverStream::new(rx))
}

fn authenticate_bearer(
    state: &CacheState,
    headers: &HeaderMap,
) -> Result<CacheTokenClaims, CacheError> {
    let raw = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| CacheError::Unauthorized("missing authorization header".to_owned()))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CacheError::Unauthorized("invalid bearer authorization header".to_owned())
        })?;
    verify_cache_token(&state.config.signing_key, token, state.clock.now_unix())
}

fn check_content_length_limit(headers: &HeaderMap, max_bytes: u64) -> Result<(), CacheError> {
    if let Some(raw) = headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok())
        && let Ok(declared) = raw.parse::<u64>()
        && declared > max_bytes
    {
        return Err(CacheError::PayloadTooLarge(format!(
            "request body ({declared} bytes) exceeds limit ({max_bytes} bytes)"
        )));
    }
    Ok(())
}

async fn read_bounded_body(body: Body, max_bytes: u64) -> Result<Bytes, CacheError> {
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(next) = stream.next().await {
        let chunk = next
            .map_err(|err| CacheError::BadRequest(format!("failed reading request body: {err}")))?;
        let next_len = (buf.len() as u64).saturating_add(chunk.len() as u64);
        if next_len > max_bytes {
            return Err(CacheError::PayloadTooLarge(format!(
                "request body exceeds limit ({max_bytes} bytes)"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Parses an Azure Blob Storage `BlockList` XML payload (`<Latest>`, `<Uncommitted>`, `<Committed>`).
pub(crate) fn parse_block_list_xml(xml: &[u8]) -> Result<Vec<String>, CacheError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_block_tag = false;
    let mut block_ids = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.local_name();
                if matches!(name.as_ref(), b"Latest" | b"Uncommitted" | b"Committed") {
                    in_block_tag = true;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.local_name();
                if matches!(name.as_ref(), b"Latest" | b"Uncommitted" | b"Committed") {
                    in_block_tag = false;
                }
            }
            Ok(Event::Text(e)) if in_block_tag => {
                let text = e
                    .unescape()
                    .map_err(|err| CacheError::BadRequest(format!("invalid blocklist xml: {err}")))?
                    .trim()
                    .to_owned();
                if !text.is_empty() {
                    validate_block_id(&text)?;
                    block_ids.push(text);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(CacheError::BadRequest(format!(
                    "malformed blocklist xml: {err}"
                )));
            }
        }
        buf.clear();
    }

    if block_ids.is_empty() {
        return Err(CacheError::BadRequest(
            "blocklist xml contained no block entries".to_owned(),
        ));
    }
    Ok(block_ids)
}

fn parse_single_byte_range(header: &str, total_size: u64) -> Result<(u64, u64), CacheError> {
    if total_size == 0 {
        return Err(CacheError::RangeNotSatisfiable(
            "cannot serve range for empty blob".to_owned(),
        ));
    }
    let spec = header
        .trim()
        .strip_prefix("bytes=")
        .ok_or_else(|| CacheError::RangeNotSatisfiable("unsupported range unit".to_owned()))?;
    if spec.contains(',') {
        return Err(CacheError::RangeNotSatisfiable(
            "multipart byte ranges are not supported".to_owned(),
        ));
    }
    let (start_str, end_str) = spec
        .split_once('-')
        .ok_or_else(|| CacheError::RangeNotSatisfiable("malformed byte range".to_owned()))?;

    let last_byte = total_size.saturating_sub(1);
    let (start, end) = if start_str.is_empty() {
        let suffix: u64 = end_str
            .parse()
            .map_err(|_| CacheError::RangeNotSatisfiable("invalid suffix range".to_owned()))?;
        if suffix == 0 {
            return Err(CacheError::RangeNotSatisfiable(
                "suffix length must be > 0".to_owned(),
            ));
        }
        (total_size.saturating_sub(suffix), last_byte)
    } else {
        let start: u64 = start_str
            .parse()
            .map_err(|_| CacheError::RangeNotSatisfiable("invalid range start".to_owned()))?;
        let end = if end_str.is_empty() {
            last_byte
        } else {
            end_str
                .parse::<u64>()
                .map_err(|_| CacheError::RangeNotSatisfiable("invalid range end".to_owned()))?
                .min(last_byte)
        };
        (start, end)
    };

    if start > last_byte || start > end {
        return Err(CacheError::RangeNotSatisfiable(
            "requested byte range out of bounds".to_owned(),
        ));
    }
    Ok((start, end))
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrString {
        Num(u64),
        Str(String),
    }

    let opt = Option::<NumOrString>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(NumOrString::Num(n)) => Ok(Some(n)),
        Some(NumOrString::Str(s)) => s
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}
