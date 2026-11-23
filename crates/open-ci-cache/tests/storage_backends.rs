//! Storage backend tests: `LocalFsStorage` LRU eviction and `S3CompatibleStorage` `SigV4` contract.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use open_ci_cache::{CacheStorage, LocalFsStorage, S3CompatibleStorage, S3Config};
use open_ci_core::{ManualClock, SecretString};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn local_fs_storage_evicts_least_recently_used_blobs_when_over_budget() -> TestResult {
    let dir = TempDir::new()?;
    let clock = Arc::new(ManualClock::new(1_700_000_000));
    // Budget allows at most 200 bytes (2 x 100-byte blobs).
    let storage =
        LocalFsStorage::open_with_clock(dir.path().to_path_buf(), 200, clock.clone()).await?;

    storage
        .put_blob("blob-oldest", Bytes::from(vec![b'1'; 100]))
        .await?;
    clock.advance(10);
    storage
        .put_blob("blob-middle", Bytes::from(vec![b'2'; 100]))
        .await?;
    assert_eq!(storage.total_bytes().await, 200);

    // Touch "blob-oldest" so "blob-middle" becomes the least-recently-used entry!
    clock.advance(10);
    let loaded = storage.get_blob("blob-oldest").await?;
    assert!(loaded.is_some());

    // Inserting a 3rd 100-byte blob exceeds the 200-byte budget -> "blob-middle" must be evicted.
    clock.advance(10);
    storage
        .put_blob("blob-newest", Bytes::from(vec![b'3'; 100]))
        .await?;

    assert_eq!(storage.total_bytes().await, 200);
    assert!(storage.head_blob("blob-oldest").await?.is_some());
    assert!(storage.head_blob("blob-middle").await?.is_none());
    assert!(storage.head_blob("blob-newest").await?.is_some());

    // Verify atomic files have strict 0o600 permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir.path().join("blobs").join("blob-newest"))?;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    Ok(())
}

#[derive(Debug, Default)]
struct MockS3State {
    objects: HashMap<String, Bytes>,
    recorded_auth_headers: Vec<(String, String, String, String)>,
}

async fn mock_s3_put(
    State(state): State<Arc<Mutex<MockS3State>>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record_sigv4(&state, "PUT", &path, &headers).await;
    state.lock().await.objects.insert(path, body);
    StatusCode::OK.into_response()
}

async fn mock_s3_get(
    State(state): State<Arc<Mutex<MockS3State>>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    record_sigv4(&state, "GET", &path, &headers).await;
    if let Some(data) = state.lock().await.objects.get(&path).cloned() {
        (StatusCode::OK, data).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn mock_s3_head(
    State(state): State<Arc<Mutex<MockS3State>>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    record_sigv4(&state, "HEAD", &path, &headers).await;
    if let Some(data) = state.lock().await.objects.get(&path) {
        let len = data.len().to_string();
        (StatusCode::OK, [(header::CONTENT_LENGTH, len)]).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn mock_s3_delete(
    State(state): State<Arc<Mutex<MockS3State>>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    record_sigv4(&state, "DELETE", &path, &headers).await;
    state.lock().await.objects.remove(&path);
    StatusCode::NO_CONTENT.into_response()
}

async fn record_sigv4(
    state: &Arc<Mutex<MockS3State>>,
    method: &str,
    path: &str,
    headers: &HeaderMap,
) {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let amz_date = headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let payload_hash = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    state.lock().await.recorded_auth_headers.push((
        format!("{method} /{path}"),
        auth,
        amz_date,
        payload_hash,
    ));
}

#[tokio::test]
async fn s3_compatible_storage_signs_sigv4_for_put_head_get_delete_and_blocks() -> TestResult {
    let mock_state = Arc::new(Mutex::new(MockS3State::default()));
    let app = Router::new()
        .route(
            "/{*path}",
            put(mock_s3_put)
                .get(mock_s3_get)
                .head(mock_s3_head)
                .delete(mock_s3_delete),
        )
        .with_state(Arc::clone(&mock_state));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let clock = Arc::new(ManualClock::new(1_700_000_000));
    let s3 = S3CompatibleStorage::new(
        S3Config {
            endpoint: format!("http://{addr}"),
            region: "us-east-1".to_owned(),
            bucket: "ci-cache-bucket".to_owned(),
            prefix: "prod/v2".to_owned(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            session_token: None,
        },
        clock,
    )?;

    // PUT -> HEAD -> GET -> DELETE contract
    let size = s3
        .put_blob("blob-s3-1", Bytes::from_static(b"fast-s3-artifact"))
        .await?;
    assert_eq!(size, 16);
    assert_eq!(s3.head_blob("blob-s3-1").await?, Some(16));

    let fetched = s3
        .get_blob("blob-s3-1")
        .await?
        .ok_or("expected blob from mock s3")?;
    assert_eq!(
        fetched.into_bytes().await?,
        Bytes::from_static(b"fast-s3-artifact")
    );

    s3.delete_blob("blob-s3-1").await?;
    assert_eq!(s3.head_blob("blob-s3-1").await?, None);

    // Block staging & commit over S3
    s3.stage_block("blob-s3-2", "blk1", Bytes::from_static(b"part1-"))
        .await?;
    s3.stage_block("blob-s3-2", "blk2", Bytes::from_static(b"part2"))
        .await?;
    let committed_size = s3
        .commit_blocks("blob-s3-2", &["blk1".to_owned(), "blk2".to_owned()], 1024)
        .await?;
    assert_eq!(committed_size, 11);
    let assembled = s3
        .get_blob("blob-s3-2")
        .await?
        .ok_or("expected assembled s3 blob")?;
    assert_eq!(
        assembled.into_bytes().await?,
        Bytes::from_static(b"part1-part2")
    );

    // Verify SigV4 headers on recorded requests
    let recorded = mock_state.lock().await.recorded_auth_headers.clone();
    assert!(recorded.len() >= 4);
    for (verb_path, auth, amz_date, payload_hash) in &recorded {
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231114/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
            ),
            "unexpected SigV4 header for {verb_path}: {auth}"
        );
        assert_eq!(amz_date, "20231114T221320Z");
        assert_eq!(payload_hash.len(), 64);
    }
    assert!(
        recorded
            .iter()
            .any(|(vp, _, _, _)| vp == "PUT /ci-cache-bucket/prod/v2/blobs/blob-s3-1")
    );
    assert!(
        recorded
            .iter()
            .any(|(vp, _, _, _)| vp == "HEAD /ci-cache-bucket/prod/v2/blobs/blob-s3-1")
    );
    assert!(
        recorded
            .iter()
            .any(|(vp, _, _, _)| vp == "GET /ci-cache-bucket/prod/v2/blobs/blob-s3-1")
    );
    assert!(
        recorded
            .iter()
            .any(|(vp, _, _, _)| vp == "DELETE /ci-cache-bucket/prod/v2/blobs/blob-s3-1")
    );

    server_handle.abort();
    Ok(())
}
