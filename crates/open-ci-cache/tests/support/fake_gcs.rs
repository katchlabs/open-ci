//! In-process loopback fake of the Cloud Storage JSON API surface `GcsBlobStore` depends on:
//! media upload, object metadata/media reads with `Range`, delete, paginated list, and compose
//! with GCS's 32-source limit and generation preconditions. Every request needs the bearer token.

use axum::body::{Body, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub const MAX_COMPOSE_SOURCES: usize = 32;
const LIST_PAGE_SIZE: usize = 3;

/// Running fake server; the listener task is aborted on drop.
#[derive(Debug)]
pub struct FakeGcs {
    pub endpoint: String,
    pub bucket: String,
    pub token: String,
    state: Arc<FakeState>,
    server: JoinHandle<()>,
}

#[derive(Debug)]
struct FakeState {
    bucket: String,
    token: String,
    objects: Mutex<BTreeMap<String, StoredObject>>,
    generations: AtomicU64,
    compose_calls: AtomicUsize,
    fail_deletes: AtomicBool,
}

#[derive(Clone, Debug)]
struct StoredObject {
    data: Bytes,
    generation: u64,
}

type HandlerResult = Result<Response, GcsFailure>;

/// JSON API error envelope: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
struct GcsFailure {
    status: StatusCode,
    message: String,
}

impl IntoResponse for GcsFailure {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.status.as_u16(), "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

impl FakeGcs {
    pub async fn start() -> std::io::Result<Self> {
        let bucket = "open-ci-fake-bucket".to_owned();
        let token = format!("fake-gcs-token-{}", uuid::Uuid::new_v4());
        let state = Arc::new(FakeState {
            bucket: bucket.clone(),
            token: token.clone(),
            objects: Mutex::new(BTreeMap::new()),
            generations: AtomicU64::new(1),
            compose_calls: AtomicUsize::new(0),
            fail_deletes: AtomicBool::new(false),
        });
        let app = Router::new()
            .route("/upload/storage/v1/b/{bucket}/o", post(upload))
            .route("/storage/v1/b/{bucket}/o", get(list))
            .route(
                "/storage/v1/b/{bucket}/o/{object}",
                get(get_object).delete(delete_object),
            )
            .route("/storage/v1/b/{bucket}/o/{object}/compose", post(compose))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("fake gcs server stopped: {err}");
            }
        });
        Ok(Self {
            endpoint,
            bucket,
            token,
            state,
            server,
        })
    }

    pub fn object_names(&self) -> Result<Vec<String>, String> {
        let objects = self.state.objects().map_err(|_| "object map poisoned")?;
        Ok(objects.keys().cloned().collect())
    }

    pub fn compose_calls(&self) -> usize {
        self.state.compose_calls.load(Ordering::SeqCst)
    }

    /// Makes every object `DELETE` answer `503` until turned off.
    pub fn fail_deletes(&self, failing: bool) {
        self.state.fail_deletes.store(failing, Ordering::SeqCst);
    }
}

impl Drop for FakeGcs {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeState {
    fn objects(&self) -> Result<MutexGuard<'_, BTreeMap<String, StoredObject>>, GcsFailure> {
        self.objects.lock().map_err(|_| {
            gcs_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "object map poisoned".to_owned(),
            )
        })
    }

    fn authorize(&self, headers: &HeaderMap, bucket: &str) -> Result<(), GcsFailure> {
        let expected = format!("Bearer {}", self.token);
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if presented != Some(expected.as_str()) {
            return Err(gcs_error(
                StatusCode::UNAUTHORIZED,
                "Invalid Credentials".to_owned(),
            ));
        }
        if bucket != self.bucket {
            return Err(gcs_error(
                StatusCode::NOT_FOUND,
                "The specified bucket does not exist.".to_owned(),
            ));
        }
        Ok(())
    }

    fn store(&self, name: &str, data: Bytes) -> HandlerResult {
        let generation = self.generations.fetch_add(1, Ordering::SeqCst);
        let object = StoredObject { data, generation };
        let resource = self.resource(name, &object);
        self.objects()?.insert(name.to_owned(), object);
        Ok(Json(resource).into_response())
    }

    fn resource(&self, name: &str, object: &StoredObject) -> Value {
        json!({
            "kind": "storage#object",
            "bucket": self.bucket,
            "name": name,
            "size": object.data.len().to_string(),
            "generation": object.generation.to_string(),
            "contentType": "application/octet-stream",
        })
    }
}

fn gcs_error(status: StatusCode, message: impl Into<String>) -> GcsFailure {
    GcsFailure {
        status,
        message: message.into(),
    }
}

fn not_found(name: &str) -> GcsFailure {
    gcs_error(StatusCode::NOT_FOUND, format!("No such object: {name}"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    upload_type: String,
    name: String,
}

async fn upload(
    State(state): State<Arc<FakeState>>,
    Path(bucket): Path<String>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> HandlerResult {
    state.authorize(&headers, &bucket)?;
    if query.upload_type != "media" {
        return Err(gcs_error(
            StatusCode::BAD_REQUEST,
            "unsupported uploadType".to_owned(),
        ));
    }
    if headers.get(header::CONTENT_TYPE)
        != Some(&HeaderValue::from_static("application/octet-stream"))
    {
        return Err(gcs_error(
            StatusCode::BAD_REQUEST,
            "expected application/octet-stream".to_owned(),
        ));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            gcs_error(
                StatusCode::LENGTH_REQUIRED,
                "Content-Length required".to_owned(),
            )
        })?;
    let data = to_bytes(body, usize::MAX)
        .await
        .map_err(|err| gcs_error(StatusCode::BAD_REQUEST, format!("body aborted: {err}")))?;
    if data.len() != declared {
        return Err(gcs_error(
            StatusCode::BAD_REQUEST,
            "body length mismatch".to_owned(),
        ));
    }
    state.store(&query.name, data)
}

#[derive(Debug, Deserialize)]
struct GetQuery {
    alt: Option<String>,
}

async fn get_object(
    State(state): State<Arc<FakeState>>,
    Path((bucket, name)): Path<(String, String)>,
    Query(query): Query<GetQuery>,
    headers: HeaderMap,
) -> HandlerResult {
    state.authorize(&headers, &bucket)?;
    let object = state
        .objects()?
        .get(&name)
        .cloned()
        .ok_or_else(|| not_found(&name))?;
    if query.alt.as_deref() != Some("media") {
        return Ok(Json(state.resource(&name, &object)).into_response());
    }
    let Some(range) = headers.get(header::RANGE) else {
        return Ok(object.data.into_response());
    };
    let total = object.data.len();
    let (start, end) = range
        .to_str()
        .ok()
        .and_then(|raw| parse_range(raw, total))
        .ok_or_else(|| {
            gcs_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "invalid range".to_owned(),
            )
        })?;
    let content_range = format!("bytes {start}-{end}/{total}");
    Ok((
        StatusCode::PARTIAL_CONTENT,
        [(header::CONTENT_RANGE, content_range)],
        object.data.slice(start..=end),
    )
        .into_response())
}

fn parse_range(raw: &str, total: usize) -> Option<(usize, usize)> {
    let (start, end) = raw.strip_prefix("bytes=")?.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let last = total.checked_sub(1)?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<usize>().ok()?.min(last)
    };
    (start <= end).then_some((start, end))
}

async fn delete_object(
    State(state): State<Arc<FakeState>>,
    Path((bucket, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> HandlerResult {
    state.authorize(&headers, &bucket)?;
    if state.fail_deletes.load(Ordering::SeqCst) {
        return Err(GcsFailure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "injected delete failure".to_owned(),
        });
    }
    match state.objects()?.remove(&name) {
        Some(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        None => Err(not_found(&name)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    prefix: Option<String>,
    page_token: Option<String>,
}

async fn list(
    State(state): State<Arc<FakeState>>,
    Path(bucket): Path<String>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> HandlerResult {
    state.authorize(&headers, &bucket)?;
    let prefix = query.prefix.unwrap_or_default();
    let objects = state.objects()?;
    let mut matching = objects
        .iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .filter(|(name, _)| query.page_token.as_ref().is_none_or(|after| *name > after));
    let page: Vec<Value> = matching
        .by_ref()
        .take(LIST_PAGE_SIZE)
        .map(|(name, object)| state.resource(name, object))
        .collect();
    let mut body = json!({ "kind": "storage#objects" });
    if matching.next().is_some()
        && let Some(last) = page.last().and_then(|item| item.get("name")).cloned()
    {
        body["nextPageToken"] = last;
    }
    if !page.is_empty() {
        body["items"] = Value::Array(page);
    }
    Ok(Json(body).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComposeBody {
    source_objects: Vec<ComposeSource>,
    destination: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComposeSource {
    name: String,
    object_preconditions: Option<Preconditions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preconditions {
    if_generation_match: Value,
}

async fn compose(
    State(state): State<Arc<FakeState>>,
    Path((bucket, destination)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<ComposeBody>,
) -> HandlerResult {
    state.authorize(&headers, &bucket)?;
    state.compose_calls.fetch_add(1, Ordering::SeqCst);
    let count = request.source_objects.len();
    if count == 0 || count > MAX_COMPOSE_SOURCES {
        return Err(gcs_error(
            StatusCode::BAD_REQUEST,
            format!(
                "The number of source components provided ({count}) exceeds the maximum ({MAX_COMPOSE_SOURCES})"
            ),
        ));
    }
    if request.destination.get("contentType") != Some(&json!("application/octet-stream")) {
        return Err(gcs_error(
            StatusCode::BAD_REQUEST,
            "destination contentType required".to_owned(),
        ));
    }
    let mut joined = Vec::new();
    {
        let objects = state.objects()?;
        for source in &request.source_objects {
            let object = objects
                .get(&source.name)
                .ok_or_else(|| not_found(&source.name))?;
            if let Some(pre) = &source.object_preconditions
                && generation_of(&pre.if_generation_match) != Some(object.generation)
            {
                return Err(gcs_error(
                    StatusCode::PRECONDITION_FAILED,
                    format!("generation mismatch for {}", source.name),
                ));
            }
            joined.extend_from_slice(&object.data);
        }
    }
    state.store(&destination, joined.into())
}

fn generation_of(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
}
