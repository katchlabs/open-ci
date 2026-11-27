//! Pure-Rust HTTP/1.1-over-Unix-domain-socket client and DTOs for Cloud Hypervisor.

use open_ci_provider::{InstanceStatus, ProviderError};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[cfg(unix)]
use std::fmt::Write as _;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
const MAX_RESPONSE_BYTES: usize = 1_048_576;
#[cfg(unix)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cloud Hypervisor `PUT /api/v1/vm.create` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub cpus: CpusConfig,
    pub memory: MemoryConfig,
    pub payload: PayloadConfig,
    pub disks: Vec<DiskConfig>,
    pub net: Vec<NetConfig>,
    pub rng: RngConfig,
    pub serial: ConsoleConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpusConfig {
    pub boot_vcpus: u32,
    pub max_vcpus: u32,
    pub topology: CpuTopology,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affinity: Vec<CpuAffinity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuAffinity {
    pub vcpu: u32,
    pub host_cpus: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTopology {
    pub threads_per_core: u32,
    pub cores_per_die: u32,
    pub dies_per_package: u32,
    pub packages: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub size: u64,
    pub hugepages: bool,
    #[serde(default)]
    pub prefault: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadConfig {
    pub kernel: String,
    pub cmdline: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskConfig {
    pub path: String,
    pub readonly: bool,
    pub direct: bool,
    pub num_queues: u32,
    pub queue_size: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_affinity: Vec<DiskQueueAffinity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskQueueAffinity {
    pub queue_index: u32,
    pub host_cpus: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetConfig {
    pub tap: String,
    pub mac: String,
    pub num_queues: u32,
    pub queue_size: u32,
    #[serde(default)]
    pub offload_tso: bool,
    #[serde(default)]
    pub offload_ufo: bool,
    #[serde(default)]
    pub offload_csum: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RngConfig {
    pub src: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Subset of `GET /api/v1/vm.info` returned by Cloud Hypervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInfoResponse {
    pub state: String,
}

/// Maps a Cloud Hypervisor `vm.info` state string to [`InstanceStatus`].
pub fn parse_vm_state(state: &str) -> InstanceStatus {
    match state {
        "Running" => InstanceStatus::Running,
        "Created" => InstanceStatus::Provisioning,
        "Paused" => InstanceStatus::Stopping,
        _ => InstanceStatus::Terminated,
    }
}

/// Typed errors from the Unix-socket Cloud Hypervisor REST transport.
#[derive(Debug, Error)]
pub enum ChApiError {
    #[error("cloud_hypervisor requires unix")]
    UnsupportedPlatform,

    #[error("socket I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cloud-hypervisor request timed out")]
    Timeout,

    #[error("cloud-hypervisor response exceeded 1 MiB limit")]
    ResponseTooLarge,

    #[error("malformed cloud-hypervisor HTTP response: {0}")]
    MalformedResponse(String),

    #[error("cloud-hypervisor API returned HTTP {status}: {detail}")]
    HttpStatus { status: u16, detail: String },

    #[error("invalid cloud-hypervisor JSON payload: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<ChApiError> for ProviderError {
    fn from(err: ChApiError) -> Self {
        match err {
            ChApiError::UnsupportedPlatform => {
                Self::InvalidConfig("cloud_hypervisor requires unix".to_owned())
            }
            ChApiError::Timeout => Self::Timeout,
            ChApiError::HttpStatus { status: 404, .. } => Self::NotFound,
            ChApiError::HttpStatus {
                status: 400 | 422,
                detail,
            } => Self::InvalidConfig(detail),
            ChApiError::HttpStatus { status, detail } => {
                Self::Transient(format!("cloud-hypervisor API status {status}: {detail}"))
            }
            ChApiError::Io(io_err)
                if matches!(
                    io_err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                Self::NotFound
            }
            other => Self::Transient(other.to_string()),
        }
    }
}

pub async fn vm_create(socket_path: &Path, config: &VmConfig) -> Result<(), ProviderError> {
    let payload = serde_json::to_vec(config).map_err(ChApiError::from)?;
    send_request(socket_path, "PUT", "/api/v1/vm.create", Some(&payload)).await?;
    Ok(())
}

pub async fn vm_boot(socket_path: &Path) -> Result<(), ProviderError> {
    send_request(socket_path, "PUT", "/api/v1/vm.boot", None).await?;
    Ok(())
}

pub async fn vm_info(socket_path: &Path) -> Result<VmInfoResponse, ProviderError> {
    let body = send_request(socket_path, "GET", "/api/v1/vm.info", None).await?;
    serde_json::from_slice(&body)
        .map_err(ChApiError::from)
        .map_err(ProviderError::from)
}

pub async fn vm_shutdown(socket_path: &Path) -> Result<(), ProviderError> {
    send_request(socket_path, "PUT", "/api/v1/vm.shutdown", None).await?;
    Ok(())
}

pub async fn vmm_shutdown(socket_path: &Path) -> Result<(), ProviderError> {
    send_request(socket_path, "PUT", "/api/v1/vmm.shutdown", None).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn send_request(
    _socket_path: &Path,
    _method: &str,
    _path: &str,
    _body: Option<&[u8]>,
) -> Result<Vec<u8>, ChApiError> {
    Err(ChApiError::UnsupportedPlatform)
}

#[cfg(unix)]
async fn send_request(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, ChApiError> {
    let request_bytes = build_http_request(method, path, body);
    let reply = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut stream = UnixStream::connect(socket_path).await?;
        stream.write_all(&request_bytes).await?;
        stream.flush().await?;
        read_http_response(&mut stream).await
    })
    .await
    .map_err(|_| ChApiError::Timeout)??;

    if (200..300).contains(&reply.status) {
        return Ok(reply.body);
    }

    let detail_slice = &reply.body[..reply.body.len().min(256)];
    let detail = String::from_utf8_lossy(detail_slice).trim().to_owned();
    Err(ChApiError::HttpStatus {
        status: reply.status,
        detail,
    })
}

#[cfg(unix)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

#[cfg(unix)]
struct ResponseHead {
    status: u16,
    content_length: Option<usize>,
    chunked: bool,
}

#[cfg(unix)]
fn build_http_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let mut head = String::with_capacity(128);
    let _ = write!(
        head,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n"
    );
    if let Some(payload) = body {
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(payload);
        out
    } else if method == "PUT" || method == "POST" {
        head.push_str("Content-Length: 0\r\n\r\n");
        head.into_bytes()
    } else {
        head.push_str("\r\n");
        head.into_bytes()
    }
}

#[cfg(unix)]
async fn read_http_response<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<HttpResponse, ChApiError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];

    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Err(ChApiError::MalformedResponse(
                "EOF before HTTP header terminator".to_owned(),
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(ChApiError::ResponseTooLarge);
        }
    };

    let head = parse_response_head(&buf[..header_end])?;
    let body_start = header_end + 4;

    if head.status == 204 || head.status == 304 || head.content_length == Some(0) {
        return Ok(HttpResponse {
            status: head.status,
            body: Vec::new(),
        });
    }

    if let Some(len) = head.content_length {
        if len > MAX_RESPONSE_BYTES {
            return Err(ChApiError::ResponseTooLarge);
        }
        let total = body_start
            .checked_add(len)
            .ok_or(ChApiError::ResponseTooLarge)?;
        while buf.len() < total {
            let read = reader.read(&mut chunk).await?;
            if read == 0 {
                return Err(ChApiError::MalformedResponse(
                    "truncated HTTP response body".to_owned(),
                ));
            }
            buf.extend_from_slice(&chunk[..read]);
            if buf.len().saturating_sub(body_start) > MAX_RESPONSE_BYTES {
                return Err(ChApiError::ResponseTooLarge);
            }
        }
        return Ok(HttpResponse {
            status: head.status,
            body: buf[body_start..total].to_vec(),
        });
    }

    if head.chunked {
        loop {
            if let Some(decoded) = decode_chunked_body(&buf[body_start..])? {
                return Ok(HttpResponse {
                    status: head.status,
                    body: decoded,
                });
            }
            let read = reader.read(&mut chunk).await?;
            if read == 0 {
                return Err(ChApiError::MalformedResponse(
                    "truncated chunked HTTP response".to_owned(),
                ));
            }
            buf.extend_from_slice(&chunk[..read]);
            if buf.len() > MAX_RESPONSE_BYTES {
                return Err(ChApiError::ResponseTooLarge);
            }
        }
    }

    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len().saturating_sub(body_start) > MAX_RESPONSE_BYTES {
            return Err(ChApiError::ResponseTooLarge);
        }
    }
    Ok(HttpResponse {
        status: head.status,
        body: buf[body_start..].to_vec(),
    })
}

#[cfg(unix)]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(unix)]
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|window| window == b"\r\n")
}

#[cfg(unix)]
fn parse_response_head(head_bytes: &[u8]) -> Result<ResponseHead, ChApiError> {
    let head_str = std::str::from_utf8(head_bytes)
        .map_err(|_| ChApiError::MalformedResponse("non-UTF-8 HTTP headers".to_owned()))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| ChApiError::MalformedResponse("missing HTTP status line".to_owned()))?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| ChApiError::MalformedResponse("missing HTTP version".to_owned()))?;
    if !version.starts_with("HTTP/1.") {
        return Err(ChApiError::MalformedResponse(format!(
            "unsupported HTTP version `{version}`"
        )));
    }
    let status_token = parts
        .next()
        .ok_or_else(|| ChApiError::MalformedResponse("missing HTTP status code".to_owned()))?;
    let status: u16 = status_token
        .parse()
        .map_err(|_| ChApiError::MalformedResponse("invalid HTTP status code".to_owned()))?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let trimmed_name = name.trim();
            let trimmed_val = value.trim();
            if trimmed_name.eq_ignore_ascii_case("content-length") {
                let parsed: usize = trimmed_val.parse().map_err(|_| {
                    ChApiError::MalformedResponse("invalid Content-Length header".to_owned())
                })?;
                content_length = Some(parsed);
            } else if trimmed_name.eq_ignore_ascii_case("transfer-encoding")
                && trimmed_val.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }

    Ok(ResponseHead {
        status,
        content_length,
        chunked,
    })
}

#[cfg(unix)]
fn decode_chunked_body(raw: &[u8]) -> Result<Option<Vec<u8>>, ChApiError> {
    let mut out = Vec::new();
    let mut pos = 0_usize;

    loop {
        let rest = raw.get(pos..).unwrap_or_default();
        let Some(line_end) = find_crlf(rest) else {
            return Ok(None);
        };
        let size_line = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| ChApiError::MalformedResponse("invalid chunk size line".to_owned()))?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| ChApiError::MalformedResponse("invalid chunk hex size".to_owned()))?;

        let data_start = pos
            .checked_add(line_end)
            .and_then(|n| n.checked_add(2))
            .ok_or(ChApiError::ResponseTooLarge)?;

        if chunk_size == 0 {
            let trailer = raw.get(data_start..).unwrap_or_default();
            if trailer.starts_with(b"\r\n") || find_header_end(trailer).is_some() {
                return Ok(Some(out));
            }
            return Ok(None);
        }

        let new_len = out
            .len()
            .checked_add(chunk_size)
            .ok_or(ChApiError::ResponseTooLarge)?;
        if new_len > MAX_RESPONSE_BYTES {
            return Err(ChApiError::ResponseTooLarge);
        }

        let data_end = data_start
            .checked_add(chunk_size)
            .ok_or(ChApiError::ResponseTooLarge)?;
        let next_pos = data_end
            .checked_add(2)
            .ok_or(ChApiError::ResponseTooLarge)?;
        if raw.len() < next_pos {
            return Ok(None);
        }
        if &raw[data_end..next_pos] != b"\r\n" {
            return Err(ChApiError::MalformedResponse(
                "missing CRLF after chunk payload".to_owned(),
            ));
        }
        out.extend_from_slice(&raw[data_start..data_end]);
        pos = next_pos;
    }
}
