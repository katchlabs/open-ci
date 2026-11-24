//! In-process loopback S3 speaking the real REST wire shapes `S3BlobStore` uses: path-style
//! objects, ranged GET, multipart upload/copy/complete/abort, and `ListObjectsV2`. It rejects
//! requests without a well-formed `SigV4` `Authorization`, `x-amz-date`, and payload hash header.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use quick_xml::Reader;
use quick_xml::escape::escape;
use quick_xml::events::Event;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const MIN_PART_BYTES: usize = 5 * 1024 * 1024;
const MAX_PART_NUMBER: u32 = 10_000;
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Identity and bucket the fake accepts, plus its `ListObjectsV2` page size.
#[derive(Clone, Debug)]
pub struct FakeS3Options {
    pub access_key_id: String,
    pub region: String,
    pub bucket: String,
    pub session_token: Option<String>,
    pub list_page_size: usize,
}

/// Running fake; the server task stops when this is dropped.
#[derive(Debug)]
pub struct FakeS3 {
    endpoint: String,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

#[derive(Debug)]
struct Shared {
    options: FakeS3Options,
    store: Mutex<Store>,
}

#[derive(Debug, Default)]
struct Store {
    objects: BTreeMap<String, StoredObject>,
    uploads: HashMap<String, PendingUpload>,
    next_upload: u64,
    operations: Vec<&'static str>,
    ok_status_faults: Vec<&'static str>,
}

#[derive(Clone, Debug)]
struct StoredObject {
    data: Bytes,
    etag: String,
}

#[derive(Debug)]
struct PendingUpload {
    key: String,
    parts: BTreeMap<u32, StoredObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    PutObject,
    UploadPart,
    UploadPartCopy,
    CreateMultipartUpload,
    CompleteMultipartUpload,
    AbortMultipartUpload,
    HeadObject,
    GetObject,
    DeleteObject,
    ListObjectsV2,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::PutObject => "PutObject",
            Self::UploadPart => "UploadPart",
            Self::UploadPartCopy => "UploadPartCopy",
            Self::CreateMultipartUpload => "CreateMultipartUpload",
            Self::CompleteMultipartUpload => "CompleteMultipartUpload",
            Self::AbortMultipartUpload => "AbortMultipartUpload",
            Self::HeadObject => "HeadObject",
            Self::GetObject => "GetObject",
            Self::DeleteObject => "DeleteObject",
            Self::ListObjectsV2 => "ListObjectsV2",
        }
    }

    fn classify(
        method: &Method,
        key: &str,
        query: &BTreeMap<String, String>,
        headers: &HeaderMap,
    ) -> Option<Self> {
        let has = |name: &str| query.contains_key(name);
        let op = match (method.as_str(), key.is_empty()) {
            ("GET", true) if query.get("list-type").map(String::as_str) == Some("2") => {
                Self::ListObjectsV2
            }
            ("PUT", false) if has("uploadId") && has("partNumber") => {
                if headers.contains_key("x-amz-copy-source") {
                    Self::UploadPartCopy
                } else {
                    Self::UploadPart
                }
            }
            ("PUT", false) if !headers.contains_key("x-amz-copy-source") => Self::PutObject,
            ("POST", false) if has("uploads") => Self::CreateMultipartUpload,
            ("POST", false) if has("uploadId") => Self::CompleteMultipartUpload,
            ("DELETE", false) if has("uploadId") => Self::AbortMultipartUpload,
            ("DELETE", false) => Self::DeleteObject,
            ("HEAD", false) => Self::HeadObject,
            ("GET", false) => Self::GetObject,
            _ => return None,
        };
        Some(op)
    }
}

impl FakeS3 {
    pub async fn start(options: FakeS3Options) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let shared = Arc::new(Shared {
            options,
            store: Mutex::new(Store::default()),
        });
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("fake s3 server stopped: {err}");
            }
        });
        Ok(Self {
            endpoint,
            shared,
            server,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn object_keys(&self) -> Vec<String> {
        self.shared
            .store
            .lock()
            .await
            .objects
            .keys()
            .cloned()
            .collect()
    }

    pub async fn open_uploads(&self) -> usize {
        self.shared.store.lock().await.uploads.len()
    }

    /// Drains the log of S3 operation names served so far.
    pub async fn take_operations(&self) -> Vec<&'static str> {
        std::mem::take(&mut self.shared.store.lock().await.operations)
    }

    /// Makes the next `operation` answer HTTP 200 with an `<Error>` document, as S3 may for
    /// `UploadPartCopy` and `CompleteMultipartUpload`.
    pub async fn fail_next_with_ok_status(&self, operation: &'static str) {
        self.shared
            .store
            .lock()
            .await
            .ok_status_faults
            .push(operation);
    }
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if let Err(reason) = check_signature_shape(&shared.options, &parts.headers) {
        return xml_error(StatusCode::FORBIDDEN, "AccessDenied", &reason);
    }
    let path = parts.uri.path().trim_start_matches('/');
    let (raw_bucket, raw_key) = path.split_once('/').unwrap_or((path, ""));
    let (Some(bucket), Some(key)) = (percent_decode(raw_bucket), percent_decode(raw_key)) else {
        return xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "bad percent-encoding",
        );
    };
    if bucket != shared.options.bucket {
        return xml_error(StatusCode::NOT_FOUND, "NoSuchBucket", "unknown bucket");
    }
    let Some(query) = parse_query(parts.uri.query().unwrap_or_default()) else {
        return xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "bad query encoding",
        );
    };
    let Some(operation) = Operation::classify(&parts.method, &key, &query, &parts.headers) else {
        return xml_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "unsupported request",
        );
    };

    let body = if matches!(parts.method, Method::PUT | Method::POST) {
        match read_body(&parts.headers, body).await {
            Ok(body) => body,
            Err(fault) => return fault.response(),
        }
    } else {
        Bytes::new()
    };

    let mut store = shared.store.lock().await;
    store.operations.push(operation.name());
    if let Some(index) = store
        .ok_status_faults
        .iter()
        .position(|name| *name == operation.name())
    {
        store.ok_status_faults.remove(index);
        return xml(
            StatusCode::OK,
            "<Error><Code>InternalError</Code><Message>injected</Message></Error>".to_owned(),
        );
    }
    let ctx = Ctx {
        options: &shared.options,
        key,
        query,
        headers: &parts.headers,
        body,
    };
    match operation {
        Operation::PutObject => put_object(&mut store, ctx),
        Operation::UploadPart => upload_part(&mut store, ctx),
        Operation::UploadPartCopy => upload_part_copy(&mut store, &ctx),
        Operation::CreateMultipartUpload => create_upload(&mut store, &ctx),
        Operation::CompleteMultipartUpload => complete_upload(&mut store, &ctx),
        Operation::AbortMultipartUpload => abort_upload(&mut store, &ctx),
        Operation::HeadObject => head_object(&store, &ctx),
        Operation::GetObject => get_object(&store, &ctx),
        Operation::DeleteObject => {
            store.objects.remove(&ctx.key);
            empty(StatusCode::NO_CONTENT)
        }
        Operation::ListObjectsV2 => list_objects(&store, &ctx),
    }
}

struct Ctx<'a> {
    options: &'a FakeS3Options,
    key: String,
    query: BTreeMap<String, String>,
    headers: &'a HeaderMap,
    body: Bytes,
}

impl Ctx<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

fn put_object(store: &mut Store, ctx: Ctx<'_>) -> Response {
    let object = stored(ctx.body);
    let etag = object.etag.clone();
    store.objects.insert(ctx.key, object);
    with_etag(empty(StatusCode::OK), &etag)
}

fn upload_part(store: &mut Store, ctx: Ctx<'_>) -> Response {
    let (upload_id, number) = match part_target(store, &ctx) {
        Ok(target) => target,
        Err(fault) => return fault.response(),
    };
    let object = stored(ctx.body);
    let etag = object.etag.clone();
    if let Some(upload) = store.uploads.get_mut(&upload_id) {
        upload.parts.insert(number, object);
    }
    with_etag(empty(StatusCode::OK), &etag)
}

fn upload_part_copy(store: &mut Store, ctx: &Ctx<'_>) -> Response {
    let (upload_id, number) = match part_target(store, ctx) {
        Ok(target) => target,
        Err(fault) => return fault.response(),
    };
    let source = ctx
        .header("x-amz-copy-source")
        .and_then(|raw| raw.strip_prefix('/'))
        .and_then(|raw| raw.split_once('/'))
        .and_then(|(bucket, key)| Some((percent_decode(bucket)?, percent_decode(key)?)));
    let Some((bucket, key)) = source else {
        return xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "bad copy source",
        );
    };
    let Some(object) = store
        .objects
        .get(&key)
        .filter(|_| bucket == ctx.options.bucket)
        .cloned()
    else {
        return xml_error(StatusCode::NOT_FOUND, "NoSuchKey", "copy source missing");
    };
    if ctx
        .header("x-amz-copy-source-if-match")
        .is_some_and(|expected| expected != object.etag)
    {
        return xml_error(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "etag mismatch",
        );
    }
    let etag = object.etag.clone();
    if let Some(upload) = store.uploads.get_mut(&upload_id) {
        upload.parts.insert(number, object);
    }
    xml(
        StatusCode::OK,
        format!(
            "<CopyPartResult><LastModified>2026-01-01T00:00:00.000Z</LastModified><ETag>{}</ETag></CopyPartResult>",
            escape(etag.as_str())
        ),
    )
}

fn part_target(store: &Store, ctx: &Ctx<'_>) -> Result<(String, u32), Fault> {
    let number = ctx
        .param("partNumber")
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|n| (1..=MAX_PART_NUMBER).contains(n))
        .ok_or({
            Fault(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "bad part number",
            )
        })?;
    let upload_id = ctx.param("uploadId").unwrap_or_default().to_owned();
    match store.uploads.get(&upload_id) {
        Some(upload) if upload.key == ctx.key => Ok((upload_id, number)),
        _ => Err(Fault(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "unknown upload",
        )),
    }
}

fn create_upload(store: &mut Store, ctx: &Ctx<'_>) -> Response {
    store.next_upload += 1;
    let upload_id = format!("upload/{}+id=", store.next_upload);
    store.uploads.insert(
        upload_id.clone(),
        PendingUpload {
            key: ctx.key.clone(),
            parts: BTreeMap::new(),
        },
    );
    xml(
        StatusCode::OK,
        format!(
            "<InitiateMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId></InitiateMultipartUploadResult>",
            escape(ctx.options.bucket.as_str()),
            escape(ctx.key.as_str()),
            escape(upload_id.as_str())
        ),
    )
}

fn complete_upload(store: &mut Store, ctx: &Ctx<'_>) -> Response {
    let upload_id = ctx.param("uploadId").unwrap_or_default().to_owned();
    let Some(upload) = store.uploads.get(&upload_id).filter(|u| u.key == ctx.key) else {
        return xml_error(StatusCode::NOT_FOUND, "NoSuchUpload", "unknown upload");
    };
    let Some(requested) = std::str::from_utf8(&ctx.body)
        .ok()
        .and_then(parse_complete_parts)
    else {
        return xml_error(StatusCode::BAD_REQUEST, "MalformedXML", "bad part list");
    };
    if requested.is_empty() || requested.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidPartOrder",
            "parts must ascend",
        );
    }
    let mut chosen = Vec::with_capacity(requested.len());
    for (number, etag) in &requested {
        match upload.parts.get(number) {
            Some(part) if part.etag == *etag => chosen.push(part.data.clone()),
            _ => {
                return xml_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "part missing or etag mismatch",
                );
            }
        }
    }
    if chosen
        .iter()
        .take(chosen.len() - 1)
        .any(|part| part.len() < MIN_PART_BYTES)
    {
        return xml_error(
            StatusCode::BAD_REQUEST,
            "EntityTooSmall",
            "non-last part under 5 MiB",
        );
    }
    let data: Bytes = chosen.concat().into();
    let etag = format!(
        "\"{}-{}\"",
        &hex::encode(Sha256::digest(&data))[..32],
        chosen.len()
    );
    store.uploads.remove(&upload_id);
    store.objects.insert(
        ctx.key.clone(),
        StoredObject {
            data,
            etag: etag.clone(),
        },
    );
    xml(
        StatusCode::OK,
        format!(
            "<CompleteMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
            escape(ctx.options.bucket.as_str()),
            escape(ctx.key.as_str()),
            escape(etag.as_str())
        ),
    )
}

fn abort_upload(store: &mut Store, ctx: &Ctx<'_>) -> Response {
    let upload_id = ctx.param("uploadId").unwrap_or_default();
    match store.uploads.get(upload_id) {
        Some(upload) if upload.key == ctx.key => {
            store.uploads.remove(upload_id);
            empty(StatusCode::NO_CONTENT)
        }
        _ => xml_error(StatusCode::NOT_FOUND, "NoSuchUpload", "unknown upload"),
    }
}

fn head_object(store: &Store, ctx: &Ctx<'_>) -> Response {
    let Some(object) = store.objects.get(&ctx.key) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let mut response = with_etag(empty(StatusCode::OK), &object.etag);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, object.data.len().into());
    response
}

fn get_object(store: &Store, ctx: &Ctx<'_>) -> Response {
    let Some(object) = store.objects.get(&ctx.key) else {
        return xml_error(StatusCode::NOT_FOUND, "NoSuchKey", "no such key");
    };
    if ctx
        .header("if-match")
        .is_some_and(|expected| expected != object.etag)
    {
        return xml_error(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "etag mismatch",
        );
    }
    let Some(range) = ctx.header("range") else {
        return with_etag(
            bytes_response(StatusCode::OK, object.data.clone()),
            &object.etag,
        );
    };
    let len = object.data.len();
    let bounds = range
        .strip_prefix("bytes=")
        .and_then(|spec| spec.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)))
        .filter(|(start, end)| start <= end && *start < len);
    let Some((start, end)) = bounds else {
        return xml_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "range not satisfiable",
        );
    };
    let end = end.min(len - 1);
    let mut response = with_etag(
        bytes_response(StatusCode::PARTIAL_CONTENT, object.data.slice(start..=end)),
        &object.etag,
    );
    if let Ok(value) = format!("bytes {start}-{end}/{len}").parse() {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

fn list_objects(store: &Store, ctx: &Ctx<'_>) -> Response {
    let prefix = ctx.param("prefix").unwrap_or_default();
    let after = ctx.param("continuation-token");
    let mut matching = store
        .objects
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .filter(|(key, _)| after.is_none_or(|token| key.as_str() > token));
    let page: Vec<_> = matching.by_ref().take(ctx.options.list_page_size).collect();
    let truncated = matching.next().is_some();
    let contents: String = page
        .iter()
        .map(|(key, object)| {
            format!(
                "<Contents><Key>{}</Key><Size>{}</Size><ETag>{}</ETag></Contents>",
                escape(key.as_str()),
                object.data.len(),
                escape(object.etag.as_str())
            )
        })
        .collect::<Vec<_>>()
        .concat();
    let next = match page.last() {
        Some((key, _)) if truncated => format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            escape(key.as_str())
        ),
        _ => String::new(),
    };
    xml(
        StatusCode::OK,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><IsTruncated>{truncated}</IsTruncated>{contents}{next}</ListBucketResult>",
            escape(ctx.options.bucket.as_str()),
            escape(prefix),
            page.len()
        ),
    )
}

fn check_signature_shape(options: &FakeS3Options, headers: &HeaderMap) -> Result<(), String> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let amz_date = header("x-amz-date").ok_or("missing x-amz-date")?;
    let date_ok = amz_date.len() == 16
        && amz_date.bytes().enumerate().all(|(i, b)| match i {
            8 => b == b'T',
            15 => b == b'Z',
            _ => b.is_ascii_digit(),
        });
    if !date_ok {
        return Err("malformed x-amz-date".to_owned());
    }
    let payload = header("x-amz-content-sha256").ok_or("missing x-amz-content-sha256")?;
    if payload != UNSIGNED_PAYLOAD && !is_lower_hex(payload, 64) {
        return Err("malformed x-amz-content-sha256".to_owned());
    }
    let authorization = header("authorization").ok_or("missing authorization")?;
    let fields: Vec<&str> = authorization
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("unsupported authorization scheme")?
        .split(", ")
        .collect();
    let [credential, signed_headers, signature] = fields.as_slice() else {
        return Err("authorization must have Credential, SignedHeaders, Signature".to_owned());
    };
    let credential = credential
        .strip_prefix("Credential=")
        .ok_or("missing Credential")?;
    let signed_headers = signed_headers
        .strip_prefix("SignedHeaders=")
        .ok_or("missing SignedHeaders")?;
    let signature = signature
        .strip_prefix("Signature=")
        .ok_or("missing Signature")?;

    let scope: Vec<&str> = credential.split('/').collect();
    let [access_key_id, date, region, "s3", "aws4_request"] = scope.as_slice() else {
        return Err("malformed credential scope".to_owned());
    };
    if *access_key_id != options.access_key_id || *region != options.region {
        return Err("unknown access key or wrong region".to_owned());
    }
    if amz_date.get(..8) != Some(*date) {
        return Err("credential date does not match x-amz-date".to_owned());
    }

    let signed: Vec<&str> = signed_headers.split(';').collect();
    let canonical_order = signed.windows(2).all(|pair| pair[0] < pair[1])
        && signed
            .iter()
            .all(|name| !name.is_empty() && *name == name.to_ascii_lowercase());
    if !canonical_order {
        return Err("SignedHeaders must be sorted lowercase names".to_owned());
    }
    for required in ["host", "x-amz-content-sha256", "x-amz-date"] {
        if !signed.contains(&required) {
            return Err(format!("{required} must be signed"));
        }
    }
    if let Some(missing) = signed.iter().find(|name| header(name).is_none()) {
        return Err(format!("signed header {missing} not sent"));
    }
    if let Some(token) = &options.session_token
        && (header("x-amz-security-token") != Some(token.as_str())
            || !signed.contains(&"x-amz-security-token"))
    {
        return Err("missing or unsigned session token".to_owned());
    }
    if !is_lower_hex(signature, 64) {
        return Err("signature must be 64 lowercase hex characters".to_owned());
    }
    Ok(())
}

async fn read_body(headers: &HeaderMap, body: Body) -> Result<Bytes, Fault> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.parse::<usize>().ok())
        .ok_or({
            Fault(
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "content-length required",
            )
        })?;
    let data = axum::body::to_bytes(body, usize::MAX).await.map_err(|_| {
        Fault(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "body ended early",
        )
    })?;
    if data.len() != declared {
        return Err(Fault(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "length mismatch",
        ));
    }
    let payload = headers
        .get("x-amz-content-sha256")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if payload != UNSIGNED_PAYLOAD && payload != hex::encode(Sha256::digest(&data)) {
        return Err(Fault(
            StatusCode::BAD_REQUEST,
            "XAmzContentSHA256Mismatch",
            "payload hash mismatch",
        ));
    }
    Ok(data)
}

fn parse_complete_parts(xml: &str) -> Option<Vec<(u32, String)>> {
    let mut reader = Reader::from_str(xml);
    let mut current = String::new();
    let mut number = None;
    let mut etag = None;
    let mut parts = Vec::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(start) => {
                current = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
            }
            Event::Text(text) => {
                let value = text.unescape().ok()?.into_owned();
                match current.as_str() {
                    "PartNumber" => number = Some(value.parse::<u32>().ok()?),
                    "ETag" => etag = Some(value),
                    _ => {}
                }
            }
            Event::End(end) => {
                if end.local_name().as_ref() == b"Part" {
                    parts.push((number.take()?, etag.take()?));
                }
                current.clear();
            }
            Event::Eof => return Some(parts),
            _ => {}
        }
    }
}

fn stored(data: Bytes) -> StoredObject {
    let etag = format!("\"{}\"", &hex::encode(Sha256::digest(&data))[..32]);
    StoredObject { data, etag }
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn percent_decode(raw: &str) -> Option<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = tail.get(..2)?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            out.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(out).ok()
}

fn parse_query(raw: &str) -> Option<BTreeMap<String, String>> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(name)?, percent_decode(value)?))
        })
        .collect()
}

fn empty(status: StatusCode) -> Response {
    bytes_response(status, Bytes::new())
}

fn bytes_response(status: StatusCode, data: Bytes) -> Response {
    let mut response = Response::new(Body::from(data));
    *response.status_mut() = status;
    response
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = etag.parse() {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn xml(status: StatusCode, body: String) -> Response {
    let mut response = bytes_response(status, Bytes::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/xml"),
    );
    response
}

/// Deferred S3 error document, small enough to return through `Result`.
#[derive(Debug)]
struct Fault(StatusCode, &'static str, &'static str);

impl Fault {
    fn response(self) -> Response {
        xml_error(self.0, self.1, self.2)
    }
}

fn xml_error(status: StatusCode, code: &str, message: &str) -> Response {
    xml(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{}</Message></Error>",
            escape(message)
        ),
    )
}
