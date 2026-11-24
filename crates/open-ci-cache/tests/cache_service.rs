//! Cache Service v2 over HTTP with real RS256 runtime tokens, a local blob store, and `SQLite`.

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::post;
use open_ci_cache::{
    BlobStore, CacheConfig, CacheIndex, CacheServer, LocalBlobStore, RetentionPolicy,
};
use open_ci_core::config::RepositoryAllowlist;
use open_ci_core::{Clock, ManualClock, SecretString, SystemClock};
use open_ci_github::test_issuer::TestIssuer;
use open_ci_github::{RuntimeTokenPolicy, RuntimeTokenVerifier};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

const BASE_URL: &str = "http://cache.internal:8090";
const SERVICE: &str = "/twirp/github.actions.results.api.v1.CacheService";
const REPO: u64 = 1001;
const MAIN: &str = "refs/heads/main";
const PR: &str = "refs/pull/12/merge";
const PUSH_TO_MAIN: &[(&str, u32)] = &[(MAIN, 3)];
const PULL_REQUEST: &[(&str, u32)] = &[(MAIN, 1), (PR, 3)];
const READ_ONLY: &[(&str, u32)] = &[(MAIN, 1)];

struct Harness {
    app: Router,
    server: CacheServer,
    issuer: TestIssuer,
    clock: Arc<ManualClock>,
    store: Arc<LocalBlobStore>,
    _dir: TempDir,
}

struct Options {
    max_entry_bytes: u64,
    max_total_bytes: u64,
    upstream: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_entry_bytes: 64 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            upstream: "http://127.0.0.1:9".to_owned(),
        }
    }
}

async fn harness(options: Options) -> AnyResult<Harness> {
    let dir = TempDir::new()?;
    let issuer = TestIssuer::start().await?;
    let clock = Arc::new(ManualClock::new(SystemClock.now_unix()));
    let store = Arc::new(LocalBlobStore::open(dir.path().join("blobs")).await?);
    let index = CacheIndex::open(&dir.path().join("index.db")).await?;
    let verifier = RuntimeTokenVerifier::new(
        RuntimeTokenPolicy {
            issuer: issuer.issuer().to_owned(),
            jwks_url: None,
            audience: None,
            repositories: RepositoryAllowlist::Ids(BTreeSet::from([REPO])),
        },
        reqwest::Client::new(),
    );
    let config = CacheConfig {
        listen_addr: "127.0.0.1:0".to_owned(),
        public_base_url: BASE_URL.to_owned(),
        signing_key: SecretString::new("s".repeat(32)),
        max_entry_bytes: options.max_entry_bytes,
        signed_url_ttl_seconds: 300,
        upstream_results_url: options.upstream,
        retention: RetentionPolicy {
            max_total_bytes: options.max_total_bytes,
            max_unused_days: 7,
        },
        sweep_interval: Duration::from_secs(300),
    };
    let server = CacheServer::new(config, store.clone(), index, verifier, clock.clone())?;
    Ok(Harness {
        app: server.router(),
        server,
        issuer,
        clock,
        store,
        _dir: dir,
    })
}

impl Harness {
    fn token(&self, grants: &[(&str, u32)]) -> AnyResult<String> {
        Ok(self.issuer.runtime_token(REPO, grants)?)
    }

    async fn rpc(
        &self,
        method: &str,
        token: Option<&str>,
        body: &Value,
    ) -> AnyResult<(StatusCode, Value)> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!("{SERVICE}/{method}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(serde_json::to_vec(body)?))?)
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    async fn put(&self, url: &str, data: Bytes) -> AnyResult<StatusCode> {
        let request = Request::builder()
            .method(Method::PUT)
            .uri(path_of(url)?)
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::from(data))?;
        Ok(self.app.clone().oneshot(request).await?.status())
    }

    async fn get(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> AnyResult<(StatusCode, HeaderMap, Bytes)> {
        let mut request = Request::builder().method(Method::GET).uri(path_of(url)?);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty())?)
            .await?;
        let (status, headers) = (response.status(), response.headers().clone());
        Ok((
            status,
            headers,
            to_bytes(response.into_body(), usize::MAX).await?,
        ))
    }

    /// Saves like the Azure SDK: staged blocks, then a block list, then finalize.
    async fn save(&self, token: &str, key: &str, data: &[u8]) -> AnyResult<StatusCode> {
        let (status, created) = self
            .rpc(
                "CreateCacheEntry",
                Some(token),
                &json!({"key": key, "version": "v1"}),
            )
            .await?;
        if status != StatusCode::OK {
            return Ok(status);
        }
        let upload_url = created["signed_upload_url"].as_str().ok_or("upload url")?;
        let mut block_list = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for (i, chunk) in data.chunks(1024).enumerate() {
            let block = format!("YmxvY2st{i:04}");
            let status = self
                .put(
                    &format!("{upload_url}&comp=block&blockid={block}"),
                    Bytes::copy_from_slice(chunk),
                )
                .await?;
            assert_eq!(status, StatusCode::CREATED);
            block_list += &["<Latest>", &block, "</Latest>"].concat();
        }
        block_list.push_str("</BlockList>");
        let status = self
            .put(
                &format!("{upload_url}&comp=blocklist"),
                Bytes::from(block_list),
            )
            .await?;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = self
            .rpc(
                "FinalizeCacheEntryUpload",
                Some(token),
                &json!({"key": key, "version": "v1", "sizeBytes": data.len().to_string()}),
            )
            .await?;
        Ok(status)
    }

    async fn restore(
        &self,
        token: &str,
        key: &str,
        restore_keys: &[&str],
    ) -> AnyResult<Option<(String, Bytes)>> {
        let (status, found) = self
            .rpc(
                "GetCacheEntryDownloadURL",
                Some(token),
                &json!({"key": key, "restoreKeys": restore_keys, "version": "v1"}),
            )
            .await?;
        assert_eq!(status, StatusCode::OK);
        if found["ok"] != json!(true) {
            return Ok(None);
        }
        let url = found["signed_download_url"]
            .as_str()
            .ok_or("download url")?;
        let (status, _, body) = self.get(url, None).await?;
        assert_eq!(status, StatusCode::OK);
        Ok(Some((
            found["matched_key"]
                .as_str()
                .ok_or("matched key")?
                .to_owned(),
            body,
        )))
    }
}

fn path_of(url: &str) -> AnyResult<String> {
    Ok(url
        .strip_prefix(BASE_URL)
        .ok_or("url not on cache origin")?
        .to_owned())
}

fn archive(len: usize, seed: u8) -> Vec<u8> {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|b| seed.wrapping_add(b))
        .collect()
}

#[tokio::test]
async fn chunked_save_then_exact_and_ranged_restore_roundtrip() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let data = archive(5000, 3);
    assert_eq!(
        h.save(&push, "deps-linux-abc", &data).await?,
        StatusCode::OK
    );

    let restored = h.restore(&push, "deps-linux-abc", &[]).await?;
    assert_eq!(
        restored,
        Some(("deps-linux-abc".to_owned(), Bytes::from(data.clone())))
    );

    let (_, found) = h
        .rpc(
            "GetCacheEntryDownloadURL",
            Some(&push),
            &json!({"key": "deps-linux-abc", "version": "v1"}),
        )
        .await?;
    let url = found["signed_download_url"].as_str().ok_or("url")?;
    let (status, headers, body) = h.get(url, Some("bytes=100-199")).await?;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 100-199/5000");
    assert_eq!(body, Bytes::copy_from_slice(&data[100..200]));
    Ok(())
}

#[tokio::test]
async fn pull_requests_read_base_ref_but_write_only_their_own_ref() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let pr = h.token(PULL_REQUEST)?;
    let main_data = archive(2000, 1);
    assert_eq!(h.save(&push, "deps-abc", &main_data).await?, StatusCode::OK);

    let from_pr = h.restore(&pr, "deps-abc", &[]).await?;
    assert_eq!(
        from_pr.map(|(_, body)| body),
        Some(Bytes::from(main_data.clone())),
        "PR falls back to main"
    );

    let poisoned = archive(2000, 99);
    assert_eq!(
        h.save(&pr, "deps-abc", &poisoned).await?,
        StatusCode::OK,
        "PR saves into its own ref"
    );
    assert_eq!(
        h.restore(&push, "deps-abc", &[])
            .await?
            .map(|(_, body)| body),
        Some(Bytes::from(main_data)),
        "main never sees the PR's entry"
    );
    assert_eq!(
        h.restore(&pr, "deps-abc", &[]).await?.map(|(_, body)| body),
        Some(Bytes::from(poisoned)),
        "the PR prefers its own ref"
    );
    Ok(())
}

#[tokio::test]
async fn entries_are_immutable_and_require_write_permission() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    assert_eq!(
        h.save(&push, "deps", &archive(10, 1)).await?,
        StatusCode::OK
    );
    assert_eq!(
        h.save(&push, "deps", &archive(10, 2)).await?,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.save(&h.token(READ_ONLY)?, "other", &archive(10, 1))
            .await?,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn untrusted_tokens_are_rejected() -> TestResult {
    let h = harness(Options::default()).await?;
    let body = json!({"key": "k", "version": "v1"});
    assert_eq!(
        h.rpc("CreateCacheEntry", None, &body).await?.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.rpc("CreateCacheEntry", Some("garbage"), &body).await?.0,
        StatusCode::UNAUTHORIZED
    );
    let foreign = h.issuer.runtime_token(REPO + 1, PUSH_TO_MAIN)?;
    assert_eq!(
        h.rpc("CreateCacheEntry", Some(&foreign), &body).await?.0,
        StatusCode::FORBIDDEN
    );
    let other_issuer = TestIssuer::start().await?;
    let wrong_issuer = other_issuer.runtime_token(REPO, PUSH_TO_MAIN)?;
    assert_eq!(
        h.rpc("CreateCacheEntry", Some(&wrong_issuer), &body)
            .await?
            .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn finalize_rejects_missing_and_mismatched_uploads_without_publishing() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let finalize = |size: &str| json!({"key": "k", "version": "v1", "sizeBytes": size});

    assert_eq!(
        h.rpc("FinalizeCacheEntryUpload", Some(&push), &finalize("1"))
            .await?
            .0,
        StatusCode::NOT_FOUND
    );

    let (_, created) = h
        .rpc(
            "CreateCacheEntry",
            Some(&push),
            &json!({"key": "k", "version": "v1"}),
        )
        .await?;
    let url = created["signed_upload_url"].as_str().ok_or("url")?;
    assert_eq!(
        h.put(url, Bytes::from(archive(100, 1))).await?,
        StatusCode::CREATED
    );
    assert_eq!(
        h.rpc("FinalizeCacheEntryUpload", Some(&push), &finalize("99"))
            .await?
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(h.restore(&push, "k", &[]).await?, None);
    Ok(())
}

#[tokio::test]
async fn signed_urls_are_bound_to_operation_and_expire() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let (_, created) = h
        .rpc(
            "CreateCacheEntry",
            Some(&push),
            &json!({"key": "k", "version": "v1"}),
        )
        .await?;
    let upload = created["signed_upload_url"]
        .as_str()
        .ok_or("url")?
        .to_owned();

    let tampered = upload.replace("sig=", "sig=00");
    assert_eq!(
        h.put(&tampered, Bytes::from_static(b"x")).await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.get(&upload, None).await?.0,
        StatusCode::FORBIDDEN,
        "an upload URL cannot download"
    );

    h.clock.advance(301);
    assert_eq!(
        h.put(&upload, Bytes::from_static(b"x")).await?,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn upload_urls_stop_accepting_bytes_once_finalized_or_abandoned() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let create = json!({"key": "deps", "version": "v1"});

    let (_, created) = h.rpc("CreateCacheEntry", Some(&push), &create).await?;
    let url = created["signed_upload_url"]
        .as_str()
        .ok_or("url")?
        .to_owned();
    assert_eq!(
        h.put(&url, Bytes::from_static(b"original")).await?,
        StatusCode::CREATED
    );
    let finalize = json!({"key": "deps", "version": "v1", "sizeBytes": "8"});
    assert_eq!(
        h.rpc("FinalizeCacheEntryUpload", Some(&push), &finalize)
            .await?
            .0,
        StatusCode::OK
    );

    assert_eq!(
        h.put(&url, Bytes::from_static(b"POISONED")).await?,
        StatusCode::FORBIDDEN,
        "a finalized entry cannot be rewritten through its upload URL"
    );
    assert_eq!(
        h.put(
            &format!("{url}&comp=block&blockid=YQ=="),
            Bytes::from_static(b"x")
        )
        .await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.restore(&push, "deps", &[]).await?.map(|(_, body)| body),
        Some(Bytes::from_static(b"original"))
    );

    let other = json!({"key": "never-uploaded", "version": "v1"});
    let (_, created) = h.rpc("CreateCacheEntry", Some(&push), &other).await?;
    let abandoned = created["signed_upload_url"]
        .as_str()
        .ok_or("url")?
        .to_owned();
    let finalize = json!({"key": "never-uploaded", "version": "v1"});
    assert_eq!(
        h.rpc("FinalizeCacheEntryUpload", Some(&push), &finalize)
            .await?
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.put(&abandoned, Bytes::from_static(b"late")).await?,
        StatusCode::FORBIDDEN,
        "an upload whose reservation was consumed cannot leak bytes into storage"
    );
    Ok(())
}

#[tokio::test]
async fn uploads_require_bounded_declared_length() -> TestResult {
    let h = harness(Options {
        max_entry_bytes: 1000,
        max_total_bytes: 10_000,
        ..Options::default()
    })
    .await?;
    let push = h.token(PUSH_TO_MAIN)?;
    let (_, created) = h
        .rpc(
            "CreateCacheEntry",
            Some(&push),
            &json!({"key": "k", "version": "v1"}),
        )
        .await?;
    let url = created["signed_upload_url"].as_str().ok_or("url")?;

    let without_length = Request::builder()
        .method(Method::PUT)
        .uri(path_of(url)?)
        .body(Body::from_stream(futures_util::stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))
        })))?;
    assert_eq!(
        h.app.clone().oneshot(without_length).await?.status(),
        StatusCode::LENGTH_REQUIRED
    );
    assert_eq!(
        h.put(url, Bytes::from(archive(1001, 1))).await?,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    Ok(())
}

#[tokio::test]
async fn retention_evicts_least_recently_used_entries_and_their_blobs() -> TestResult {
    let h = harness(Options {
        max_entry_bytes: 1000,
        max_total_bytes: 1000,
        ..Options::default()
    })
    .await?;
    let push = h.token(PUSH_TO_MAIN)?;
    assert_eq!(
        h.save(&push, "old", &archive(600, 1)).await?,
        StatusCode::OK
    );
    h.clock.advance(10);
    assert_eq!(
        h.save(&push, "new", &archive(600, 2)).await?,
        StatusCode::OK
    );

    assert_eq!(h.server.sweep().await?, 1);
    assert_eq!(h.restore(&push, "old", &[]).await?, None);
    assert!(h.restore(&push, "new", &[]).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn restore_skips_entries_whose_blob_disappeared() -> TestResult {
    let h = harness(Options::default()).await?;
    let push = h.token(PUSH_TO_MAIN)?;
    assert_eq!(
        h.save(&push, "deps-old", &archive(10, 1)).await?,
        StatusCode::OK
    );
    h.clock.advance(10);
    assert_eq!(
        h.save(&push, "deps-new", &archive(10, 2)).await?,
        StatusCode::OK
    );

    let (_, found) = h
        .rpc(
            "GetCacheEntryDownloadURL",
            Some(&push),
            &json!({"key": "deps-new", "version": "v1"}),
        )
        .await?;
    let url = found["signed_download_url"].as_str().ok_or("url")?;
    let blob_id = path_of(url)?
        .trim_start_matches("/v1/cache/blobs/")
        .split('?')
        .next()
        .ok_or("id")?
        .to_owned();
    h.store
        .delete(&open_ci_cache::BlobId::parse(&blob_id)?)
        .await?;

    let restored = h.restore(&push, "deps-x", &["deps-"]).await?;
    assert_eq!(restored.map(|(key, _)| key).as_deref(), Some("deps-old"));
    Ok(())
}

#[derive(Clone, Default)]
struct Recorded(Arc<Mutex<Vec<ForwardedCall>>>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ForwardedCall {
    method: String,
    authorization: Option<String>,
    body: String,
}

#[tokio::test]
async fn non_cache_twirp_calls_pass_through_to_github_results_service() -> TestResult {
    let recorded = Recorded::default();
    let upstream = Router::new()
        .route(
            "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
            post(
                |State(rec): State<Recorded>, headers: HeaderMap, body: String| async move {
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if let Ok(mut calls) = rec.0.lock() {
                        calls.push(ForwardedCall {
                            method: "CreateArtifact".to_owned(),
                            authorization: auth,
                            body,
                        });
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"ok":true,"signed_upload_url":"https://blob.example/x"}"#,
                    )
                },
            ),
        )
        .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream_url = format!("http://{}", listener.local_addr()?);
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await });

    let h = harness(Options {
        upstream: upstream_url,
        ..Options::default()
    })
    .await?;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact")
        .header(header::AUTHORIZATION, "Bearer job-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"name":"report"}"#))?;
    let response = h.app.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024).await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&body)?["signed_upload_url"],
        "https://blob.example/x"
    );
    let calls = recorded.0.lock().map_err(|_| "lock")?.clone();
    assert_eq!(
        calls,
        vec![ForwardedCall {
            method: "CreateArtifact".to_owned(),
            authorization: Some("Bearer job-token".to_owned()),
            body: r#"{"name":"report"}"#.to_owned(),
        }]
    );

    let (status, _) = h.rpc("DeleteCacheEntry", Some("t"), &json!({})).await?;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unknown cache methods are never forwarded"
    );
    upstream_task.abort();
    Ok(())
}
