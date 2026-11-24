//! Forwards non-cache Twirp calls (artifacts, step summaries) to GitHub's results service.
//!
//! Redirecting `ACTIONS_RESULTS_URL` sends every results-service client to this server, so
//! anything other than the cache RPCs must reach GitHub unchanged or `upload-artifact@v4` breaks.

use crate::error::CacheError;
use crate::http::{CacheState, TWIRP_MAX_BODY_BYTES};
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{
    ACCEPT_ENCODING, CONNECTION, EXPECT, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
    TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use axum::http::{HeaderMap, HeaderName};
use axum::response::Response;
use futures_util::TryStreamExt;
use std::sync::Arc;
use std::time::Duration;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) async fn forward(
    State(state): State<Arc<CacheState>>,
    request: Request,
) -> Result<Response, CacheError> {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path(), |pq| pq.as_str());
    let url = format!(
        "{}{path}",
        state.config.upstream_results_url.trim_end_matches('/')
    );
    let body = to_bytes(body, TWIRP_MAX_BODY_BYTES)
        .await
        .map_err(|_| CacheError::PayloadTooLarge("twirp request body too large".to_owned()))?;

    let mut headers = end_to_end(&parts.headers);
    headers.remove(ACCEPT_ENCODING);
    let upstream = state
        .upstream
        .request(parts.method, url)
        .headers(headers)
        .body(body)
        .timeout(UPSTREAM_TIMEOUT)
        .send()
        .await
        .map_err(|err| {
            CacheError::BadGateway(format!("results passthrough: {}", err.without_url()))
        })?;

    let mut response = Response::builder().status(upstream.status());
    if let Some(response_headers) = response.headers_mut() {
        *response_headers = end_to_end(upstream.headers());
    }
    response
        .body(Body::from_stream(
            upstream
                .bytes_stream()
                .map_err(|err| std::io::Error::other(err.without_url())),
        ))
        .map_err(|err| CacheError::BadGateway(format!("results passthrough: {err}")))
}

fn end_to_end(headers: &HeaderMap) -> HeaderMap {
    let hop_by_hop: [HeaderName; 9] = [
        CONNECTION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        HOST,
        EXPECT,
    ];
    headers
        .iter()
        .filter(|(name, _)| !hop_by_hop.contains(name) && name.as_str() != "keep-alive")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
