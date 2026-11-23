//! Integration tests for the GitHub Actions v2 Twirp Cache Service protocol and security controls.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use open_ci_cache::{CacheConfig, CacheServer, LocalFsStorage, issue_cache_token};
use open_ci_core::{Clock, ManualClock, SecretString};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const BASE_URL: &str = "http://cache.internal:8090";
const SIGNING_KEY: &str = "super-secret-signing-key-for-tests";
const CREATE_URL: &str = "/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry";
const FINALIZE_URL: &str =
    "/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload";
const DOWNLOAD_URL: &str =
    "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL";

async fn make_test_server(
    dir: &TempDir,
    clock: Arc<ManualClock>,
    max_entry_bytes: u64,
    max_total_bytes: u64,
) -> Result<CacheServer, Box<dyn std::error::Error>> {
    let mut cfg = CacheConfig::local_fs(
        BASE_URL,
        SecretString::new(SIGNING_KEY),
        dir.path().to_path_buf(),
    );
    cfg.max_entry_bytes = max_entry_bytes;
    cfg.max_total_bytes = max_total_bytes;
    cfg.signed_url_ttl_seconds = 300;
    let storage = Arc::new(
        LocalFsStorage::open_with_clock(dir.path().to_path_buf(), max_total_bytes, clock.clone())
            .await?,
    );
    Ok(CacheServer::with_storage(cfg, storage, clock).await?)
}

async fn send_json(
    app: &Router,
    uri: &str,
    bearer: Option<&str>,
    payload: &Value,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(tok) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let req = builder.body(Body::from(serde_json::to_vec(payload)?))?;
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 2 * 1024 * 1024).await?;
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, parsed))
}

async fn send_put(
    app: &Router,
    uri: &str,
    body: impl Into<Body>,
) -> Result<StatusCode, Box<dyn std::error::Error>> {
    let req = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .body(body.into())?;
    Ok(app.clone().oneshot(req).await?.status())
}

fn strip_base_url(signed_url: &str) -> Result<String, Box<dyn std::error::Error>> {
    signed_url
        .strip_prefix(BASE_URL)
        .map(str::to_owned)
        .ok_or_else(|| format!("unexpected base url in {signed_url}").into())
}

async fn upload_single_shot(
    app: &Router,
    token: &str,
    key: &str,
    version: &str,
    payload: &'static [u8],
) -> TestResult {
    let (status, created) = send_json(
        app,
        CREATE_URL,
        Some(token),
        &json!({ "key": key, "version": version }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let upload_path = strip_base_url(
        created["signed_upload_url"]
            .as_str()
            .ok_or("missing signed_upload_url")?,
    )?;
    assert_eq!(
        send_put(app, &upload_path, payload).await?,
        StatusCode::CREATED
    );
    let (fin_status, fin) = send_json(
        app,
        FINALIZE_URL,
        Some(token),
        &json!({ "key": key, "version": version, "size_bytes": payload.len() }),
    )
    .await?;
    assert_eq!(fin_status, StatusCode::OK);
    assert_eq!(fin["ok"], true);
    Ok(())
}

#[tokio::test]
async fn twirp_azure_chunked_upload_exact_hit_head_get_and_range() -> TestResult {
    let dir = TempDir::new()?;
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let server = make_test_server(&dir, clock.clone(), 1024 * 1024, 10 * 1024 * 1024).await?;
    let app = server.router();
    let token = issue_cache_token(
        &SecretString::new(SIGNING_KEY),
        "acme/repo-a",
        3600,
        clock.now_unix(),
    )?;

    let health = app
        .clone()
        .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
        .await?;
    assert_eq!(health.status(), StatusCode::OK);

    let (status, created) = send_json(
        &app,
        CREATE_URL,
        Some(&token),
        &json!({ "key": "linux-cargo-old", "version": "v1" }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["ok"], true);
    let upload_path = strip_base_url(
        created["signed_upload_url"]
            .as_str()
            .ok_or("missing signed_upload_url")?,
    )?;

    assert_eq!(
        send_put(
            &app,
            &format!("{upload_path}&comp=block&blockid=AAAA"),
            "hello "
        )
        .await?,
        StatusCode::CREATED
    );
    assert_eq!(
        send_put(
            &app,
            &format!("{upload_path}&comp=block&blockid=AAAB"),
            "open-ci-cache!"
        )
        .await?,
        StatusCode::CREATED
    );
    let xml = r#"<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>AAAA</Latest><Latest>AAAB</Latest></BlockList>"#;
    assert_eq!(
        send_put(&app, &format!("{upload_path}&comp=blocklist"), xml).await?,
        StatusCode::CREATED
    );

    let (fin_status, finalized) = send_json(
        &app,
        FINALIZE_URL,
        Some(&token),
        &json!({ "key": "linux-cargo-old", "version": "v1", "size_bytes": "20" }),
    )
    .await?;
    assert_eq!(fin_status, StatusCode::OK);
    assert_eq!(finalized["ok"], true);
    assert!(
        finalized["entry_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    let (hit_status, hit) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token),
        &json!({ "key": "linux-cargo-old", "restore_keys": ["linux-cargo-"], "version": "v1" }),
    )
    .await?;
    assert_eq!(hit_status, StatusCode::OK);
    assert_eq!(hit["ok"], true);
    assert_eq!(hit["matched_key"], "linux-cargo-old");
    let download_path = strip_base_url(
        hit["signed_download_url"]
            .as_str()
            .ok_or("missing signed_download_url")?,
    )?;
    verify_download_head_get_and_range(&app, &download_path).await
}

async fn verify_download_head_get_and_range(app: &Router, download_path: &str) -> TestResult {
    let head_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(download_path)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(
        head_resp
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("20")
    );

    let get_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(download_path)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(
        &axum::body::to_bytes(get_resp.into_body(), 1024).await?[..],
        b"hello open-ci-cache!"
    );

    let range_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(download_path)
                .header(header::RANGE, "bytes=6-12")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(range_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &axum::body::to_bytes(range_resp.into_body(), 1024).await?[..],
        b"open-ci"
    );
    Ok(())
}

#[tokio::test]
async fn restore_keys_prefix_selects_newest_handles_miss_and_persists_across_restart() -> TestResult
{
    let dir = TempDir::new()?;
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let server = make_test_server(&dir, clock.clone(), 1024 * 1024, 10 * 1024 * 1024).await?;
    let app = server.router();
    let token = issue_cache_token(
        &SecretString::new(SIGNING_KEY),
        "acme/repo-a",
        3600,
        clock.now_unix(),
    )?;

    upload_single_shot(&app, &token, "linux-cargo-1", "v1", b"older-cache").await?;
    clock.advance(60);
    upload_single_shot(&app, &token, "linux-cargo-2", "v1", b"newer-cache").await?;

    let (status, prefix_hit) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token),
        &json!({
            "key": "linux-cargo-missing",
            "restore_keys": ["linux-cargo-"],
            "version": "v1"
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(prefix_hit["ok"], true);
    assert_eq!(prefix_hit["matched_key"], "linux-cargo-2");

    let (miss_status, miss) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token),
        &json!({
            "key": "linux-cargo-2",
            "restore_keys": ["linux-cargo-"],
            "version": "v2-other"
        }),
    )
    .await?;
    assert_eq!(miss_status, StatusCode::OK);
    assert_eq!(miss["ok"], false);
    assert!(miss.get("signed_download_url").is_none());

    let reloaded = make_test_server(&dir, clock.clone(), 1024 * 1024, 10 * 1024 * 1024).await?;
    let (restart_status, after_restart) = send_json(
        &reloaded.router(),
        DOWNLOAD_URL,
        Some(&token),
        &json!({ "key": "linux-cargo-1", "restore_keys": [], "version": "v1" }),
    )
    .await?;
    assert_eq!(restart_status, StatusCode::OK);
    assert_eq!(after_restart["ok"], true);
    assert_eq!(after_restart["matched_key"], "linux-cargo-1");
    Ok(())
}

#[tokio::test]
async fn security_cross_scope_isolation_and_token_or_url_expiry_and_tampering() -> TestResult {
    let dir = TempDir::new()?;
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let server = make_test_server(&dir, clock.clone(), 1024, 4096).await?;
    let app = server.router();
    let key = SecretString::new(SIGNING_KEY);

    let token_a = issue_cache_token(&key, "org/repo-a", 120, clock.now_unix())?;
    let token_b = issue_cache_token(&key, "org/repo-b", 120, clock.now_unix())?;
    upload_single_shot(&app, &token_a, "shared-key", "v1", b"repo-a-secret").await?;

    // Cross-scope isolation: repo-b cannot see repo-a's entry
    let (status, other_lookup) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token_b),
        &json!({ "key": "shared-key", "restore_keys": ["shared-"], "version": "v1" }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(other_lookup["ok"], false);

    // Signed download URL tampering and operation confusion
    let (_, owner_lookup) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token_a),
        &json!({ "key": "shared-key", "restore_keys": [], "version": "v1" }),
    )
    .await?;
    let download_path = strip_base_url(
        owner_lookup["signed_download_url"]
            .as_str()
            .ok_or("missing signed_download_url")?,
    )?;

    let tampered = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("{download_path}dead"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(tampered.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        send_put(&app, &download_path, "poison").await?,
        StatusCode::FORBIDDEN
    );

    // Expired bearer token -> 401
    clock.advance(200);
    let (expired_token_status, _) = send_json(
        &app,
        DOWNLOAD_URL,
        Some(&token_a),
        &json!({ "key": "shared-key", "restore_keys": [], "version": "v1" }),
    )
    .await?;
    assert_eq!(expired_token_status, StatusCode::UNAUTHORIZED);

    // Expired signed URL -> 403
    clock.advance(200);
    let expired_dl = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&download_path)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(expired_dl.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn security_rejects_oversized_single_and_chunked_uploads_and_tampered_put_sig() -> TestResult
{
    let dir = TempDir::new()?;
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let server = make_test_server(&dir, clock.clone(), 64, 4096).await?;
    let app = server.router();
    let token = issue_cache_token(
        &SecretString::new(SIGNING_KEY),
        "org/repo-a",
        300,
        clock.now_unix(),
    )?;

    let (_, created) = send_json(
        &app,
        CREATE_URL,
        Some(&token),
        &json!({ "key": "big-blob", "version": "v1" }),
    )
    .await?;
    let upload_path = strip_base_url(
        created["signed_upload_url"]
            .as_str()
            .ok_or("missing signed_upload_url")?,
    )?;

    // Tampered upload URL signature rejected
    assert_eq!(
        send_put(&app, &format!("{upload_path}ff"), "tiny").await?,
        StatusCode::FORBIDDEN
    );

    // Oversized single-shot upload rejected with 413
    assert_eq!(
        send_put(&app, &upload_path, vec![b'x'; 65]).await?,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // Oversized assembled chunked upload rejected with 413
    assert_eq!(
        send_put(
            &app,
            &format!("{upload_path}&comp=block&blockid=B1"),
            vec![b'a'; 40]
        )
        .await?,
        StatusCode::CREATED
    );
    assert_eq!(
        send_put(
            &app,
            &format!("{upload_path}&comp=block&blockid=B2"),
            vec![b'b'; 40]
        )
        .await?,
        StatusCode::CREATED
    );
    let xml = "<BlockList><Latest>B1</Latest><Latest>B2</Latest></BlockList>";
    assert_eq!(
        send_put(&app, &format!("{upload_path}&comp=blocklist"), xml).await?,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    Ok(())
}
