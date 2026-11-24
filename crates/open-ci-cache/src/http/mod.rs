//! HTTP surface: Cache Service v2 Twirp RPCs, signed blob transfer, and results passthrough.

mod blobs;
mod passthrough;
mod twirp;

use crate::config::CacheConfig;
use crate::error::CacheError;
use crate::index::CacheIndex;
use crate::retention;
use crate::storage::BlobStore;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use open_ci_core::Clock;
use open_ci_github::RuntimeTokenVerifier;
use std::future::Future;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

pub(crate) const TWIRP_MAX_BODY_BYTES: usize = 1024 * 1024;

const CACHE_SERVICE: &str = "/twirp/github.actions.results.api.v1.CacheService";

/// Shared state behind every handler.
#[derive(Debug)]
pub struct CacheState {
    pub(crate) config: CacheConfig,
    pub(crate) store: Arc<dyn BlobStore>,
    pub(crate) index: CacheIndex,
    pub(crate) verifier: RuntimeTokenVerifier,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) upstream: reqwest::Client,
    pub(crate) sweep_requested: Notify,
}

/// GitHub Actions Cache Service v2 server.
#[derive(Debug)]
pub struct CacheServer {
    state: Arc<CacheState>,
}

impl CacheServer {
    pub fn new(
        config: CacheConfig,
        store: Arc<dyn BlobStore>,
        index: CacheIndex,
        verifier: RuntimeTokenVerifier,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, CacheError> {
        config.validate()?;
        let upstream = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| CacheError::Internal(format!("building upstream client: {err}")))?;
        Ok(Self {
            state: Arc::new(CacheState {
                config,
                store,
                index,
                verifier,
                clock,
                upstream,
                sweep_requested: Notify::new(),
            }),
        })
    }

    pub fn router(&self) -> Router {
        router(Arc::clone(&self.state))
    }

    /// Applies retention once and deletes the evicted blobs; returns how many were evicted.
    pub async fn sweep(&self) -> Result<usize, CacheError> {
        retention::sweep(&self.state).await
    }

    /// Serves until `shutdown` resolves, running retention sweeps in the background.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), CacheError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listen_addr = self.state.config.listen_addr.clone();
        let listener = TcpListener::bind(&listen_addr)
            .await
            .map_err(|err| CacheError::Internal(format!("binding {listen_addr}: {err}")))?;
        let cancel = CancellationToken::new();
        let sweeper = tokio::spawn(retention::run(Arc::clone(&self.state), cancel.clone()));
        let stop = cancel.clone();
        tokio::spawn(async move {
            shutdown.await;
            stop.cancel();
        });
        let served = axum::serve(listener, self.router())
            .with_graceful_shutdown(cancel.clone().cancelled_owned())
            .await
            .map_err(|err| CacheError::Internal(format!("cache server exited: {err}")));
        cancel.cancel();
        let _ = sweeper.await;
        served
    }
}

fn router(state: Arc<CacheState>) -> Router {
    let twirp_routes = Router::new()
        .route(
            &format!("{CACHE_SERVICE}/CreateCacheEntry"),
            post(twirp::create_cache_entry),
        )
        .route(
            &format!("{CACHE_SERVICE}/FinalizeCacheEntryUpload"),
            post(twirp::finalize_cache_entry_upload),
        )
        .route(
            &format!("{CACHE_SERVICE}/GetCacheEntryDownloadURL"),
            post(twirp::get_cache_entry_download_url),
        )
        .route(
            &format!("{CACHE_SERVICE}/{{method}}"),
            post(twirp::unsupported_method),
        )
        .route("/twirp/{*route}", post(passthrough::forward))
        .layer(DefaultBodyLimit::max(TWIRP_MAX_BODY_BYTES));

    let blob_routes = Router::new()
        .route(
            "/v1/cache/blobs/{id}",
            put(blobs::put_blob)
                .get(blobs::get_blob)
                .head(blobs::head_blob),
        )
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(twirp_routes)
        .merge(blob_routes)
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                tracing::debug_span!("cache_request", method = %request.method(), path = %request.uri().path())
            },
        ))
        .with_state(state)
}
