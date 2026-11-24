//! Signed path-style S3 REST calls for the operations [`super::S3BlobStore`] needs.

use super::sigv4::{
    self, CanonicalRequest, EMPTY_PAYLOAD_SHA256, Timestamp, UNSIGNED_PAYLOAD, canonical_query,
    uri_encode,
};
use super::xml::{XmlDocument, complete_multipart_body};
use crate::error::CacheError;
use crate::ids::ByteRange;
use crate::storage::{BlobUpload, ByteStream};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use open_ci_core::{Clock, SystemClock};
use open_ci_provider::ProviderError;
use open_ci_provider_aws::AwsCredentialsSource;
use reqwest::header::{CONTENT_LENGTH, ETAG, HOST};
use reqwest::{Method, Response, StatusCode};
use std::io;
use std::sync::{Arc, OnceLock};

/// Validated endpoint, region, and bucket plus the HTTP client and credential source.
#[derive(Debug)]
pub(super) struct S3Client {
    pub(super) http: reqwest::Client,
    pub(super) credentials: Arc<dyn AwsCredentialsSource>,
    pub(super) origin: String,
    pub(super) host: String,
    pub(super) region: String,
    pub(super) bucket: String,
}

/// Object metadata from `HEAD`.
#[derive(Clone, Debug)]
pub(super) struct ObjectHead {
    pub(super) size: u64,
    pub(super) etag: String,
}

/// One page of `ListObjectsV2`.
#[derive(Debug)]
pub(super) struct ListPage {
    pub(super) keys: Vec<String>,
    pub(super) next_token: Option<String>,
}

/// Why a streamed upload failed: its source body, or S3 itself.
enum UploadFailure {
    Source(String),
    S3(CacheError),
}

enum Payload {
    Empty,
    Signed(Bytes),
    Streaming {
        content_length: u64,
        body: reqwest::Body,
    },
}

struct S3Request<'a> {
    operation: &'static str,
    method: Method,
    key: Option<&'a str>,
    query: Vec<(&'static str, String)>,
    headers: Vec<(&'static str, String)>,
    payload: Payload,
}

impl<'a> S3Request<'a> {
    fn object(operation: &'static str, method: Method, key: &'a str) -> Self {
        Self {
            operation,
            method,
            key: Some(key),
            query: Vec::new(),
            headers: Vec::new(),
            payload: Payload::Empty,
        }
    }

    fn query(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.query.push((name, value.into()));
        self
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn payload(mut self, payload: Payload) -> Self {
        self.payload = payload;
        self
    }
}

impl S3Client {
    pub(super) async fn head_object(&self, key: &str) -> Result<Option<ObjectHead>, CacheError> {
        let request = S3Request::object("HeadObject", Method::HEAD, key);
        let operation = request.operation;
        let response = self.send(request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = ensure_success(operation, response).await?;
        let size = header_str(&response, CONTENT_LENGTH.as_str())
            .and_then(|raw| raw.parse::<u64>().ok())
            .ok_or_else(|| missing_header(operation, "content-length"))?;
        let etag = header_str(&response, ETAG.as_str())
            .ok_or_else(|| missing_header(operation, "etag"))?
            .to_owned();
        Ok(Some(ObjectHead { size, etag }))
    }

    /// Streams `upload` as the object body; a failing client body surfaces as `BadRequest`.
    pub(super) async fn put_object(&self, key: &str, upload: BlobUpload) -> Result<(), CacheError> {
        let request = S3Request::object("PutObject", Method::PUT, key);
        match self.send_upload(request, upload).await {
            Ok(_) => Ok(()),
            Err(UploadFailure::Source(reason)) => Err(CacheError::BadRequest(reason)),
            Err(UploadFailure::S3(err)) => Err(err),
        }
    }

    pub(super) async fn put_empty_object(&self, key: &str) -> Result<(), CacheError> {
        let request = S3Request::object("PutObject", Method::PUT, key);
        let operation = request.operation;
        ensure_success(operation, self.send(request).await?)
            .await
            .map(drop)
    }

    /// Opens a body stream for `key`, optionally ranged and pinned to an `etag`.
    pub(super) async fn get_object(
        &self,
        key: &str,
        range: Option<ByteRange>,
        if_match: Option<&str>,
    ) -> Result<ByteStream, CacheError> {
        let mut request = S3Request::object("GetObject", Method::GET, key);
        if let Some(range) = range {
            request = request.header("range", range.to_header_value());
        }
        if let Some(etag) = if_match {
            request = request.header("if-match", etag);
        }
        let operation = request.operation;
        let response = self.send(request).await?;
        match response.status() {
            StatusCode::NOT_FOUND => return Err(CacheError::NotFound("blob not found".to_owned())),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Err(CacheError::RangeNotSatisfiable(
                    "requested byte range out of bounds".to_owned(),
                ));
            }
            _ => {}
        }
        let response = ensure_success(operation, response).await?;
        if let Some(range) = range {
            let length = header_str(&response, CONTENT_LENGTH.as_str())
                .and_then(|raw| raw.parse::<u64>().ok());
            if length != Some(range.byte_count()) {
                return Err(CacheError::Storage(format!(
                    "s3 {operation} returned a body that does not match the requested range"
                )));
            }
        }
        Ok(response.bytes_stream().map_err(io::Error::other).boxed())
    }

    /// Deletes `key`; a missing object counts as deleted.
    pub(super) async fn delete_object(&self, key: &str) -> Result<(), CacheError> {
        let request = S3Request::object("DeleteObject", Method::DELETE, key);
        let operation = request.operation;
        let response = self.send(request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ensure_success(operation, response).await.map(drop)
    }

    pub(super) async fn list_objects(
        &self,
        prefix: &str,
        continuation_token: Option<&str>,
    ) -> Result<ListPage, CacheError> {
        let mut request = S3Request {
            operation: "ListObjectsV2",
            method: Method::GET,
            key: None,
            query: Vec::new(),
            headers: Vec::new(),
            payload: Payload::Empty,
        }
        .query("list-type", "2")
        .query("prefix", prefix);
        if let Some(token) = continuation_token {
            request = request.query("continuation-token", token);
        }
        let operation = request.operation;
        let doc = xml_result(operation, self.send(request).await?, "ListBucketResult").await?;
        let keys = doc.all("Contents/Key").map(str::to_owned).collect();
        let truncated = doc.first("IsTruncated") == Some("true");
        let next_token = doc.first("NextContinuationToken").map(str::to_owned);
        if truncated && next_token.is_none() {
            return Err(CacheError::Storage(format!(
                "s3 {operation} truncated without a continuation token"
            )));
        }
        Ok(ListPage {
            keys,
            next_token: next_token.filter(|_| truncated),
        })
    }

    pub(super) async fn create_multipart_upload(&self, key: &str) -> Result<String, CacheError> {
        let request =
            S3Request::object("CreateMultipartUpload", Method::POST, key).query("uploads", "");
        let operation = request.operation;
        let doc = xml_result(
            operation,
            self.send(request).await?,
            "InitiateMultipartUploadResult",
        )
        .await?;
        doc.first("UploadId")
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| missing_field(operation, "UploadId"))
    }

    /// Uploads one part from a streamed body and returns its `ETag`.
    pub(super) async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        upload: BlobUpload,
    ) -> Result<String, CacheError> {
        let request = S3Request::object("UploadPart", Method::PUT, key)
            .query("partNumber", part_number.to_string())
            .query("uploadId", upload_id);
        let operation = request.operation;
        let response =
            self.send_upload(request, upload)
                .await
                .map_err(|failure| match failure {
                    UploadFailure::Source(reason) => CacheError::Storage(format!(
                        "s3 {operation} source stream failed: {reason}"
                    )),
                    UploadFailure::S3(err) => err,
                })?;
        header_str(&response, ETAG.as_str())
            .map(str::to_owned)
            .ok_or_else(|| missing_header(operation, "etag"))
    }

    /// Copies the whole of `source_key` server-side as one part, pinned to `source_etag`.
    pub(super) async fn upload_part_copy(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        source_key: &str,
        source_etag: &str,
    ) -> Result<String, CacheError> {
        let copy_source = format!(
            "/{}/{}",
            uri_encode(&self.bucket, false),
            uri_encode(source_key, true)
        );
        let request = S3Request::object("UploadPartCopy", Method::PUT, key)
            .query("partNumber", part_number.to_string())
            .query("uploadId", upload_id)
            .header("x-amz-copy-source", copy_source)
            .header("x-amz-copy-source-if-match", source_etag);
        let operation = request.operation;
        let doc = xml_result(operation, self.send(request).await?, "CopyPartResult").await?;
        doc.first("ETag")
            .filter(|etag| !etag.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| missing_field(operation, "ETag"))
    }

    pub(super) async fn complete_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<(), CacheError> {
        let body = Bytes::from(complete_multipart_body(parts));
        let request = S3Request::object("CompleteMultipartUpload", Method::POST, key)
            .query("uploadId", upload_id)
            .header("content-type", "application/xml")
            .payload(Payload::Signed(body));
        let operation = request.operation;
        xml_result(
            operation,
            self.send(request).await?,
            "CompleteMultipartUploadResult",
        )
        .await
        .map(drop)
    }

    pub(super) async fn abort_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
    ) -> Result<(), CacheError> {
        let request = S3Request::object("AbortMultipartUpload", Method::DELETE, key)
            .query("uploadId", upload_id);
        let operation = request.operation;
        let response = self.send(request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ensure_success(operation, response).await.map(drop)
    }

    async fn send_upload(
        &self,
        request: S3Request<'_>,
        upload: BlobUpload,
    ) -> Result<Response, UploadFailure> {
        let operation = request.operation;
        let content_length = upload.content_length();
        let source_failure = Arc::new(OnceLock::<String>::new());
        let observer = Arc::clone(&source_failure);
        let stream = upload.into_stream().inspect_err(move |err| {
            observer.get_or_init(|| err.to_string());
        });
        let request = request.payload(Payload::Streaming {
            content_length,
            body: reqwest::Body::wrap_stream(stream),
        });
        let sent = match self.send(request).await {
            Ok(response) => ensure_success(operation, response).await,
            Err(err) => Err(err),
        };
        match (sent, source_failure.get()) {
            (Err(_), Some(reason)) => Err(UploadFailure::Source(reason.clone())),
            (sent, _) => sent.map_err(UploadFailure::S3),
        }
    }

    async fn send(&self, request: S3Request<'_>) -> Result<Response, CacheError> {
        let credentials = self
            .credentials
            .credentials()
            .await
            .map_err(|err| credential_error(&err))?;
        let timestamp = Timestamp::from_unix(SystemClock.now_unix())?;
        let canonical_uri = match request.key {
            Some(key) => format!(
                "/{}/{}",
                uri_encode(&self.bucket, false),
                uri_encode(key, true)
            ),
            None => format!("/{}", uri_encode(&self.bucket, false)),
        };
        let query = canonical_query(&request.query);
        let (payload_hash, content_length, body) = match request.payload {
            Payload::Empty => (EMPTY_PAYLOAD_SHA256.to_owned(), 0, None),
            Payload::Signed(bytes) => (
                sigv4::sha256_hex(&bytes),
                bytes.len() as u64,
                Some(reqwest::Body::from(bytes)),
            ),
            Payload::Streaming {
                content_length,
                body,
            } => (UNSIGNED_PAYLOAD.to_owned(), content_length, Some(body)),
        };

        let mut headers = vec![
            (HOST.as_str().to_owned(), self.host.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), timestamp.amz_date().to_owned()),
        ];
        if let Some(token) = &credentials.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.expose().to_owned()));
        }
        headers.extend(
            request
                .headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone())),
        );
        let authorization = sigv4::authorization(
            &credentials,
            &self.region,
            &timestamp,
            &CanonicalRequest {
                method: request.method.as_str(),
                canonical_uri: &canonical_uri,
                canonical_query: &query,
                headers: &headers,
                payload_hash: &payload_hash,
            },
        )?;

        let url = if query.is_empty() {
            format!("{}{canonical_uri}", self.origin)
        } else {
            format!("{}{canonical_uri}?{query}", self.origin)
        };
        let mut builder = self
            .http
            .request(request.method.clone(), url)
            .header("authorization", authorization);
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if matches!(request.method, Method::PUT | Method::POST) {
            builder = builder.header(CONTENT_LENGTH, content_length);
        }
        if let Some(body) = body {
            builder = builder.body(body);
        }
        builder.send().await.map_err(|err| {
            CacheError::Storage(format!(
                "s3 {} request failed: {}",
                request.operation,
                describe_transport(err)
            ))
        })
    }
}

fn credential_error(err: &ProviderError) -> CacheError {
    CacheError::Storage(format!("resolving s3 credentials: {err}"))
}

fn describe_transport(err: reqwest::Error) -> String {
    let err = err.without_url();
    let mut reason = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        reason.push_str(": ");
        reason.push_str(&cause.to_string());
        source = cause.source();
    }
    reason
}

fn header_str<'r>(response: &'r Response, name: &str) -> Option<&'r str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn missing_header(operation: &str, header: &str) -> CacheError {
    CacheError::Storage(format!("s3 {operation} response missing {header}"))
}

fn missing_field(operation: &str, field: &str) -> CacheError {
    CacheError::Storage(format!("s3 {operation} response missing {field}"))
}

async fn ensure_success(operation: &str, response: Response) -> Result<Response, CacheError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let code = response
        .text()
        .await
        .ok()
        .and_then(|body| XmlDocument::parse(&body).ok())
        .and_then(|doc| doc.first("Code").map(str::to_owned));
    Err(status_error(operation, status, code.as_deref()))
}

/// Parses a success body whose root must be `expected_root`; S3 can report failures of
/// `UploadPartCopy` and `CompleteMultipartUpload` as HTTP 200 with an `<Error>` document.
async fn xml_result(
    operation: &str,
    response: Response,
    expected_root: &str,
) -> Result<XmlDocument, CacheError> {
    let response = ensure_success(operation, response).await?;
    let status = response.status();
    let body = response.text().await.map_err(|_| {
        CacheError::Storage(format!("s3 {operation} response body could not be read"))
    })?;
    let doc = XmlDocument::parse(&body)
        .map_err(|reason| CacheError::Storage(format!("s3 {operation} response: {reason}")))?;
    if doc.root() == expected_root {
        Ok(doc)
    } else {
        Err(status_error(operation, status, doc.first("Code")))
    }
}

fn status_error(operation: &str, status: StatusCode, code: Option<&str>) -> CacheError {
    let status = status.as_u16();
    let message = match code.filter(|code| code.bytes().all(|b| b.is_ascii_alphanumeric())) {
        Some(code) => format!("s3 {operation} failed with HTTP {status} ({code})"),
        None => format!("s3 {operation} failed with HTTP {status}"),
    };
    CacheError::Storage(message)
}
