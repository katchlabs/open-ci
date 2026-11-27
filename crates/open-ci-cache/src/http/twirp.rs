//! `github.actions.results.api.v1.CacheService` RPCs, authorized by the job's runtime token.

use crate::capability::{BlobOp, sign_blob_url};
use crate::error::CacheError;
use crate::http::CacheState;
use crate::ids::BlobId;
use crate::index::{CacheEntry, EntryKey, Reservation, RestoreQuery};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use open_ci_github::{RuntimeIdentity, RuntimeTokenError};
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::Arc;

const MAX_KEY_LEN: usize = 512;
const MAX_VERSION_LEN: usize = 256;
const MAX_RESTORE_KEYS: usize = 64;
const DANGLING_ENTRY_RETRIES: usize = 3;

#[derive(Deserialize)]
pub(super) struct CreateCacheEntryRequest {
    key: String,
    version: String,
}

#[derive(Serialize)]
pub(super) struct CreateCacheEntryResponse {
    ok: bool,
    signed_upload_url: String,
}

pub(super) async fn create_cache_entry(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<CreateCacheEntryRequest>,
) -> Result<Json<CreateCacheEntryResponse>, CacheError> {
    let identity = authenticate(&state, &headers).await?;
    let entry = write_entry(&identity, req.key, req.version)?;
    let now = state.clock.now_unix();
    let ttl = state.config.signed_url_ttl_seconds;
    let url_expires = now.saturating_add(ttl);
    let blob = BlobId::generate();

    let stale_after = url_expires.saturating_add(ttl);
    let (reservation, replaced) = state
        .index
        .reserve(entry, blob.clone(), now, stale_after)
        .await?;
    if let Some(stale) = replaced {
        if let Ok(mut active) = state.active_reservations.write() {
            active.remove(&stale);
        }
        delete_quietly(&state, &stale).await;
    }
    if reservation == Reservation::AlreadyExists {
        return Err(CacheError::AlreadyExists(
            "cache entry with the same key, version, and scope already exists".to_owned(),
        ));
    }
    let signed_upload_url = sign_blob_url(
        &state.config.public_base_url,
        &state.config.signing_key,
        &blob,
        BlobOp::Put,
        url_expires,
    )?;
    if let Ok(mut active) = state.active_reservations.write() {
        active.insert(blob, stale_after);
    }
    Ok(Json(CreateCacheEntryResponse {
        ok: true,
        signed_upload_url,
    }))
}

#[derive(Deserialize)]
pub(super) struct FinalizeCacheEntryUploadRequest {
    key: String,
    version: String,
    #[serde(default, alias = "sizeBytes", deserialize_with = "optional_u64")]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct FinalizeCacheEntryUploadResponse {
    ok: bool,
    entry_id: String,
}

pub(super) async fn finalize_cache_entry_upload(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<FinalizeCacheEntryUploadRequest>,
) -> Result<Json<FinalizeCacheEntryUploadResponse>, CacheError> {
    let identity = authenticate(&state, &headers).await?;
    let entry = write_entry(&identity, req.key, req.version)?;
    let blob = state
        .index
        .take_reservation(entry.clone())
        .await?
        .ok_or_else(|| CacheError::NotFound("no pending upload for this key".to_owned()))?;
    if let Ok(mut active) = state.active_reservations.write() {
        active.remove(&blob);
    }

    let published = publish(&state, entry, &blob, req.size_bytes).await;
    if published.is_err() {
        delete_quietly(&state, &blob).await;
    }
    let committed = published?;
    state.sweep_requested.notify_one();
    Ok(Json(FinalizeCacheEntryUploadResponse {
        ok: true,
        entry_id: committed.id.to_string(),
    }))
}

#[derive(Deserialize)]
pub(super) struct GetCacheEntryDownloadUrlRequest {
    key: String,
    #[serde(default, alias = "restoreKeys")]
    restore_keys: Vec<String>,
    version: String,
}

#[derive(Default, Serialize)]
pub(super) struct GetCacheEntryDownloadUrlResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_key: Option<String>,
}

pub(super) async fn get_cache_entry_download_url(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(req): Json<GetCacheEntryDownloadUrlRequest>,
) -> Result<Json<GetCacheEntryDownloadUrlResponse>, CacheError> {
    let identity = authenticate(&state, &headers).await?;
    validate_key(&req.key)?;
    validate_version(&req.version)?;
    if req.restore_keys.len() > MAX_RESTORE_KEYS {
        return Err(CacheError::BadRequest(format!(
            "at most {MAX_RESTORE_KEYS} restore keys are allowed"
        )));
    }
    for prefix in &req.restore_keys {
        validate_key(prefix)?;
    }
    let scopes = identity.read_scopes();
    let query = RestoreQuery {
        repository_id: identity.repository_id(),
        version: &req.version,
        scopes: &scopes,
        primary_key: &req.key,
        restore_keys: &req.restore_keys,
    };

    for _ in 0..DANGLING_ENTRY_RETRIES {
        let Some(entry) = state.index.find(&query).await? else {
            break;
        };
        if state.store.size(&entry.blob).await?.is_none() {
            state.index.remove(entry.id).await?;
            continue;
        }
        let now = state.clock.now_unix();
        state.index.touch(entry.id, now).await?;
        let signed_download_url = sign_blob_url(
            &state.config.public_base_url,
            &state.config.signing_key,
            &entry.blob,
            BlobOp::Get,
            now.saturating_add(state.config.signed_url_ttl_seconds),
        )?;
        return Ok(Json(GetCacheEntryDownloadUrlResponse {
            ok: true,
            signed_download_url: Some(signed_download_url),
            matched_key: Some(entry.key.key),
        }));
    }
    Ok(Json(GetCacheEntryDownloadUrlResponse::default()))
}

pub(super) async fn unsupported_method(Path(method): Path<String>) -> CacheError {
    CacheError::NotFound(format!("CacheService/{method} is not implemented"))
}

async fn authenticate(
    state: &CacheState,
    headers: &HeaderMap,
) -> Result<RuntimeIdentity, CacheError> {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| CacheError::Unauthorized("missing bearer runtime token".to_owned()))?;
    state.verifier.verify(token).await.map_err(|err| match err {
        RuntimeTokenError::JwksUnavailable => CacheError::BadGateway(err.to_string()),
        RuntimeTokenError::RepositoryNotAllowed => CacheError::Forbidden(err.to_string()),
        _ => CacheError::Unauthorized(err.to_string()),
    })
}

fn write_entry(
    identity: &RuntimeIdentity,
    key: String,
    version: String,
) -> Result<EntryKey, CacheError> {
    validate_key(&key)?;
    validate_version(&version)?;
    let scope = identity.write_scope().ok_or_else(|| {
        CacheError::Forbidden("runtime token grants no writable cache scope".to_owned())
    })?;
    Ok(EntryKey {
        repository_id: identity.repository_id(),
        scope: scope.to_owned(),
        version,
        key,
    })
}

/// Publishes the reserved upload as an immutable entry; the caller deletes the blob on error.
async fn publish(
    state: &CacheState,
    entry: EntryKey,
    blob: &BlobId,
    declared_size: Option<u64>,
) -> Result<CacheEntry, CacheError> {
    let stored = state.store.size(blob).await?;
    let size = verified_upload_size(stored, declared_size, state.config.max_entry_bytes)?;
    state
        .index
        .commit(entry, blob.clone(), size, state.clock.now_unix())
        .await?
        .ok_or_else(|| CacheError::AlreadyExists("cache entry was saved concurrently".to_owned()))
}

fn verified_upload_size(
    stored: Option<u64>,
    declared: Option<u64>,
    max_entry_bytes: u64,
) -> Result<u64, CacheError> {
    let actual =
        stored.ok_or_else(|| CacheError::NotFound("upload was not received".to_owned()))?;
    if actual > max_entry_bytes {
        return Err(CacheError::PayloadTooLarge(format!(
            "entry exceeds {max_entry_bytes} bytes"
        )));
    }
    if declared.is_some_and(|declared| declared != actual) {
        return Err(CacheError::BadRequest(
            "uploaded size does not match declared size".to_owned(),
        ));
    }
    Ok(actual)
}

async fn delete_quietly(state: &CacheState, blob: &BlobId) {
    if let Err(err) = state.store.delete(blob).await {
        tracing::warn!(blob = %blob, error = %err, "failed deleting discarded cache blob");
    }
}

fn validate_key(key: &str) -> Result<(), CacheError> {
    validate_text("cache key", key, MAX_KEY_LEN)
}

fn validate_version(version: &str) -> Result<(), CacheError> {
    validate_text("cache version", version, MAX_VERSION_LEN)
}

fn validate_text(what: &str, value: &str, max_len: usize) -> Result<(), CacheError> {
    if value.is_empty() || value.len() > max_len || value.contains(char::is_control) {
        return Err(CacheError::BadRequest(format!(
            "{what} must be 1..={max_len} bytes without control characters"
        )));
    }
    Ok(())
}

fn optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(u64),
        Text(String),
    }
    match Option::<NumberOrString>::deserialize(deserializer)? {
        None => Ok(None),
        Some(NumberOrString::Number(n)) => Ok(Some(n)),
        Some(NumberOrString::Text(s)) => {
            s.trim().parse().map(Some).map_err(serde::de::Error::custom)
        }
    }
}
