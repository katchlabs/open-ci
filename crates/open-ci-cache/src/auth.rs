//! Scoped HMAC-SHA256 bearer tokens, signed blob URLs, and input validation.

use crate::error::CacheError;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use open_ci_core::SecretString;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const MAX_SCOPE_LEN: usize = 256;
const MAX_KEY_LEN: usize = 512;
const MAX_VERSION_LEN: usize = 256;
const MAX_BLOB_ID_LEN: usize = 128;
const MAX_BLOCK_ID_LEN: usize = 256;

/// Verified claims embedded inside a scoped cache bearer token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheTokenClaims {
    /// Tenant or repository scope (e.g. `acme/widgets`).
    pub scope: String,
    /// Expiration timestamp in UTC Unix seconds.
    pub exp: u64,
}

/// Authorized operation encoded in a signed blob capability URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobOp {
    Put,
    Get,
}

impl BlobOp {
    /// Canonical lowercase operation label used in signed URL query parameters.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Get => "get",
        }
    }
}

/// Mints a scoped HMAC-SHA256 bearer token for authenticating Twirp cache RPCs.
///
/// Format: `<base64url_no_pad(json_claims)>.<hex_hmac_sha256>`.
pub fn issue_cache_token(
    signing_key: &SecretString,
    scope: &str,
    ttl_seconds: u64,
    now_unix: u64,
) -> Result<String, CacheError> {
    if signing_key.is_empty() {
        return Err(CacheError::InvalidConfig(
            "signing key must not be empty".to_owned(),
        ));
    }
    validate_scope(scope)?;
    let claims = CacheTokenClaims {
        scope: scope.to_owned(),
        exp: now_unix.saturating_add(ttl_seconds),
    };
    let payload_json = serde_json::to_vec(&claims)
        .map_err(|err| CacheError::Internal(format!("failed serializing token claims: {err}")))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json);
    let sig_hex = compute_hmac_hex(signing_key, payload_b64.as_bytes())?;
    Ok(format!("{payload_b64}.{sig_hex}"))
}

/// Verifies a scoped HMAC-SHA256 bearer token using constant-time MAC verification.
pub fn verify_cache_token(
    signing_key: &SecretString,
    token: &str,
    now_unix: u64,
) -> Result<CacheTokenClaims, CacheError> {
    let mut parts = token.split('.');
    let payload_b64 = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CacheError::Unauthorized("malformed bearer token".to_owned()))?;
    let sig_hex = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CacheError::Unauthorized("malformed bearer token".to_owned()))?;
    if parts.next().is_some() {
        return Err(CacheError::Unauthorized(
            "malformed bearer token".to_owned(),
        ));
    }

    verify_hmac_hex(signing_key, payload_b64.as_bytes(), sig_hex)
        .map_err(|()| CacheError::Unauthorized("invalid token signature".to_owned()))?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| CacheError::Unauthorized("invalid token encoding".to_owned()))?;
    let claims: CacheTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|_| CacheError::Unauthorized("invalid token claims".to_owned()))?;
    validate_scope(&claims.scope)
        .map_err(|_| CacheError::Unauthorized("invalid token scope".to_owned()))?;

    if now_unix > claims.exp {
        return Err(CacheError::Unauthorized("token has expired".to_owned()));
    }
    Ok(claims)
}

/// Signs a capability URL for uploading or downloading a specific blob ID.
pub fn sign_blob_url(
    public_base_url: &str,
    signing_key: &SecretString,
    blob_id: &str,
    op: BlobOp,
    exp: u64,
) -> Result<String, CacheError> {
    validate_blob_id(blob_id)?;
    let msg = blob_signature_message(blob_id, op, exp);
    let sig = compute_hmac_hex(signing_key, msg.as_bytes())?;
    let base = public_base_url.trim().trim_end_matches('/');
    Ok(format!(
        "{base}/v1/cache/blobs/{blob_id}?exp={exp}&op={}&sig={sig}",
        op.as_str()
    ))
}

/// Verifies signed query parameters (`exp`, `op`, `sig`) for a blob request in constant time.
pub fn verify_blob_signature(
    signing_key: &SecretString,
    blob_id: &str,
    expected_op: BlobOp,
    provided_op: Option<&str>,
    exp: Option<u64>,
    sig_hex: Option<&str>,
    now_unix: u64,
) -> Result<(), CacheError> {
    validate_blob_id(blob_id)?;
    let op_str = provided_op
        .ok_or_else(|| CacheError::Forbidden("missing signed url operation".to_owned()))?;
    if op_str != expected_op.as_str() {
        return Err(CacheError::Forbidden(
            "signed url operation mismatch".to_owned(),
        ));
    }
    let exp_val =
        exp.ok_or_else(|| CacheError::Forbidden("missing signed url expiration".to_owned()))?;
    let sig =
        sig_hex.ok_or_else(|| CacheError::Forbidden("missing signed url signature".to_owned()))?;

    let msg = blob_signature_message(blob_id, expected_op, exp_val);
    verify_hmac_hex(signing_key, msg.as_bytes(), sig)
        .map_err(|()| CacheError::Forbidden("invalid signed url signature".to_owned()))?;

    if now_unix > exp_val {
        return Err(CacheError::Forbidden("signed url has expired".to_owned()));
    }
    Ok(())
}

/// Validates a tenant/repository scope string.
pub fn validate_scope(scope: &str) -> Result<(), CacheError> {
    if scope.is_empty() || scope.len() > MAX_SCOPE_LEN {
        return Err(CacheError::BadRequest(
            "scope must be 1..=256 characters".to_owned(),
        ));
    }
    if scope.contains("..")
        || !scope
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'\\'))
    {
        return Err(CacheError::BadRequest(
            "scope contains invalid characters".to_owned(),
        ));
    }
    Ok(())
}

/// Validates a cache key or restore-key prefix.
pub fn validate_key(key: &str) -> Result<(), CacheError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(CacheError::BadRequest(
            "cache key must be 1..=512 characters".to_owned(),
        ));
    }
    if key.chars().any(char::is_control) {
        return Err(CacheError::BadRequest(
            "cache key must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

/// Validates a cache version hash string.
pub fn validate_version(version: &str) -> Result<(), CacheError> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        return Err(CacheError::BadRequest(
            "cache version must be 1..=256 characters".to_owned(),
        ));
    }
    if version.chars().any(char::is_control) {
        return Err(CacheError::BadRequest(
            "cache version must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

/// Validates a blob identifier strictly to prevent path traversal.
pub fn validate_blob_id(blob_id: &str) -> Result<(), CacheError> {
    if blob_id.is_empty() || blob_id.len() > MAX_BLOB_ID_LEN {
        return Err(CacheError::BadRequest(
            "blob id must be 1..=128 characters".to_owned(),
        ));
    }
    if !blob_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(CacheError::BadRequest(
            "blob id contains forbidden characters".to_owned(),
        ));
    }
    Ok(())
}

/// Validates an Azure block ID query parameter.
pub fn validate_block_id(block_id: &str) -> Result<(), CacheError> {
    if block_id.is_empty() || block_id.len() > MAX_BLOCK_ID_LEN {
        return Err(CacheError::BadRequest(
            "blockid must be 1..=256 characters".to_owned(),
        ));
    }
    if block_id.chars().any(char::is_control) {
        return Err(CacheError::BadRequest(
            "blockid must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn blob_signature_message(blob_id: &str, op: BlobOp, exp: u64) -> String {
    format!("blob:{blob_id}:{}:{exp}", op.as_str())
}

fn compute_hmac_hex(signing_key: &SecretString, message: &[u8]) -> Result<String, CacheError> {
    let mut mac = HmacSha256::new_from_slice(signing_key.expose().as_bytes())
        .map_err(|_| CacheError::Internal("invalid hmac key".to_owned()))?;
    mac.update(message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn verify_hmac_hex(
    signing_key: &SecretString,
    message: &[u8],
    provided_hex: &str,
) -> Result<(), ()> {
    let provided = hex::decode(provided_hex).map_err(|_| ())?;
    let mut mac = HmacSha256::new_from_slice(signing_key.expose().as_bytes()).map_err(|_| ())?;
    mac.update(message);
    mac.verify_slice(&provided).map_err(|_| ())
}
