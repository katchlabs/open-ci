//! Signed blob endpoints speaking the Azure Block Blob subset the Actions toolkit uses.

use crate::capability::{BlobOp, SignedQuery, verify_blob_url};
use crate::error::CacheError;
use crate::http::{CacheState, TWIRP_MAX_BODY_BYTES};
use crate::ids::{BlobId, BlockId, ByteRange};
use crate::storage::BlobUpload;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct BlobQuery {
    exp: Option<u64>,
    op: Option<String>,
    sig: Option<String>,
    comp: Option<String>,
    blockid: Option<String>,
}

impl BlobQuery {
    fn signed(&self) -> SignedQuery<'_> {
        SignedQuery {
            op: self.op.as_deref(),
            exp: self.exp,
            sig: self.sig.as_deref(),
        }
    }
}

fn authorize(
    state: &CacheState,
    raw_id: &str,
    op: BlobOp,
    query: &BlobQuery,
) -> Result<BlobId, CacheError> {
    let blob = BlobId::parse(raw_id)?;
    verify_blob_url(
        &state.config.signing_key,
        &blob,
        op,
        &query.signed(),
        state.clock.now_unix(),
    )?;
    Ok(blob)
}

pub(super) async fn put_blob(
    State(state): State<Arc<CacheState>>,
    Path(raw_id): Path<String>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, CacheError> {
    let blob = authorize(&state, &raw_id, BlobOp::Put, &query)?;
    let now = state.clock.now_unix();
    let fast_accepted = state
        .active_reservations
        .read()
        .ok()
        .and_then(|active| active.get(&blob).copied())
        .is_some_and(|stale_after| stale_after > now);
    if !fast_accepted && !state.index.accepts_upload(blob.clone(), now).await? {
        return Err(CacheError::Forbidden(
            "upload is not reserved or was already finalized".to_owned(),
        ));
    }
    let max = state.config.max_entry_bytes;
    match query.comp.as_deref() {
        None => state.store.put(&blob, upload(&headers, body, max)?).await?,
        Some("block") => {
            let raw_block = query.blockid.as_deref().ok_or_else(|| {
                CacheError::BadRequest("comp=block requires a blockid".to_owned())
            })?;
            let block = BlockId::parse(raw_block)?;
            state
                .store
                .stage_block(&blob, &block, upload(&headers, body, max)?)
                .await?;
        }
        Some("blocklist") => {
            let xml = to_bytes(body, TWIRP_MAX_BODY_BYTES)
                .await
                .map_err(|_| CacheError::PayloadTooLarge("block list is too large".to_owned()))?;
            let blocks = parse_block_list(&xml)?;
            let store = Arc::clone(&state.store);
            tokio::spawn(async move { store.commit_blocks(&blob, &blocks, max).await })
                .await
                .map_err(|err| {
                    CacheError::Internal(format!("block commit task failed: {err}"))
                })??;
        }
        Some(other) => {
            return Err(CacheError::BadRequest(format!(
                "unsupported comp parameter: {other}"
            )));
        }
    }
    Ok((
        StatusCode::CREATED,
        [("x-ms-request-id", Uuid::new_v4().to_string())],
    )
        .into_response())
}

fn upload(headers: &HeaderMap, body: Body, max_bytes: u64) -> Result<BlobUpload, CacheError> {
    let length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            CacheError::LengthRequired("uploads must declare Content-Length".to_owned())
        })?;
    if length > max_bytes {
        return Err(CacheError::PayloadTooLarge(format!(
            "upload exceeds {max_bytes} bytes"
        )));
    }
    let stream = body
        .into_data_stream()
        .map_err(std::io::Error::other)
        .boxed();
    Ok(BlobUpload::new(length, stream))
}

pub(super) async fn head_blob(
    State(state): State<Arc<CacheState>>,
    Path(raw_id): Path<String>,
    Query(query): Query<BlobQuery>,
) -> Result<Response, CacheError> {
    let blob = authorize(&state, &raw_id, BlobOp::Get, &query)?;
    let size = state
        .store
        .size(&blob)
        .await?
        .ok_or_else(|| CacheError::NotFound("blob not found".to_owned()))?;
    blob_response(StatusCode::OK, Body::empty(), size, None)
}

pub(super) async fn get_blob(
    State(state): State<Arc<CacheState>>,
    Path(raw_id): Path<String>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Response, CacheError> {
    let blob = authorize(&state, &raw_id, BlobOp::Get, &query)?;
    let size = state
        .store
        .size(&blob)
        .await?
        .ok_or_else(|| CacheError::NotFound("blob not found".to_owned()))?;
    let requested = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|header| ByteRange::parse_header(header, size))
        .transpose()?;
    let Some(range) = requested.or_else(|| ByteRange::full(size)) else {
        return blob_response(StatusCode::OK, Body::empty(), 0, None);
    };
    let body = Body::from_stream(state.store.read(&blob, range).await?);
    match requested {
        Some(range) => blob_response(StatusCode::PARTIAL_CONTENT, body, size, Some(range)),
        None => blob_response(StatusCode::OK, body, size, None),
    }
}

fn blob_response(
    status: StatusCode,
    body: Body,
    size: u64,
    range: Option<ByteRange>,
) -> Result<Response, CacheError> {
    let header = |value: String| {
        HeaderValue::from_str(&value)
            .map_err(|_| CacheError::Internal("invalid response header".to_owned()))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let length = range.map_or(size, ByteRange::byte_count);
    headers.insert(CONTENT_LENGTH, header(length.to_string())?);
    if let Some(range) = range {
        headers.insert(
            CONTENT_RANGE,
            header(format!(
                "bytes {}-{}/{size}",
                range.start(),
                range.end_inclusive()
            ))?,
        );
    }
    Ok(response)
}

/// Azure's limit on committed blocks per blob; also bounds the storage calls one commit makes.
const MAX_BLOCKS: usize = 50_000;

/// Block IDs from an Azure `BlockList` body, in commit order.
fn parse_block_list(xml: &[u8]) -> Result<Vec<BlockId>, CacheError> {
    let malformed =
        |err: &dyn std::fmt::Display| CacheError::BadRequest(format!("invalid block list: {err}"));
    let is_block_tag = |name: &[u8]| matches!(name, b"Latest" | b"Uncommitted" | b"Committed");
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_block = false;
    let mut blocks = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|err| malformed(&err))?
        {
            Event::Start(tag) => in_block = is_block_tag(tag.local_name().as_ref()),
            Event::End(tag) if is_block_tag(tag.local_name().as_ref()) => in_block = false,
            Event::Text(text) if in_block => {
                if blocks.len() == MAX_BLOCKS {
                    return Err(CacheError::BadRequest(format!(
                        "block list exceeds {MAX_BLOCKS} blocks"
                    )));
                }
                let id = text.unescape().map_err(|err| malformed(&err))?;
                blocks.push(BlockId::parse(id.trim())?);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if blocks.is_empty() {
        return Err(CacheError::BadRequest("block list is empty".to_owned()));
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_list_preserves_order_across_tag_kinds() {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>AAA=</Latest><Uncommitted>BBB=</Uncommitted><Committed>CCC=</Committed></BlockList>"#;
        let ids: Result<Vec<String>, _> = parse_block_list(xml)
            .map(|blocks| blocks.iter().map(|b| b.as_str().to_owned()).collect());
        assert!(
            matches!(ids.as_deref(), Ok([a, b, c]) if a == "AAA=" && b == "BBB=" && c == "CCC=")
        );
        assert!(parse_block_list(b"<BlockList></BlockList>").is_err());
        let oversized = format!(
            "<BlockList>{}</BlockList>",
            "<Latest>AA==</Latest>".repeat(MAX_BLOCKS + 1)
        );
        assert!(parse_block_list(oversized.as_bytes()).is_err());
        assert!(parse_block_list(b"<BlockList><Latest>").is_err());
    }
}
