//! HMAC-signed, expiring capability URLs for blob upload and download.

use crate::error::CacheError;
use crate::ids::BlobId;
use hmac::{Hmac, Mac};
use open_ci_core::SecretString;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Operation a signed blob URL authorizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobOp {
    Put,
    Get,
}

impl BlobOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Get => "get",
        }
    }
}

/// Query parameters carried by a signed blob URL.
#[derive(Debug)]
pub struct SignedQuery<'a> {
    pub op: Option<&'a str>,
    pub exp: Option<u64>,
    pub sig: Option<&'a str>,
}

/// Mints `{base}/v1/cache/blobs/{blob}?exp=..&op=..&sig=..`.
pub fn sign_blob_url(
    public_base_url: &str,
    signing_key: &SecretString,
    blob: &BlobId,
    op: BlobOp,
    exp: u64,
) -> Result<String, CacheError> {
    let sig = hmac_hex(signing_key, signature_message(blob, op, exp).as_bytes())?;
    let base = public_base_url.trim().trim_end_matches('/');
    Ok(format!(
        "{base}/v1/cache/blobs/{blob}?exp={exp}&op={}&sig={sig}",
        op.as_str()
    ))
}

/// Verifies a signed blob URL for `expected` in constant time and rejects expired URLs.
pub fn verify_blob_url(
    signing_key: &SecretString,
    blob: &BlobId,
    expected: BlobOp,
    query: &SignedQuery<'_>,
    now_unix: u64,
) -> Result<(), CacheError> {
    let forbidden = |msg: &str| CacheError::Forbidden(msg.to_owned());
    if query.op != Some(expected.as_str()) {
        return Err(forbidden("signed url operation mismatch"));
    }
    let exp = query
        .exp
        .ok_or_else(|| forbidden("missing signed url expiration"))?;
    let sig = query
        .sig
        .and_then(|sig| hex::decode(sig).ok())
        .ok_or_else(|| forbidden("missing or malformed signed url signature"))?;
    let mut mac = HmacSha256::new_from_slice(signing_key.expose().as_bytes())
        .map_err(|_| CacheError::Internal("invalid hmac key".to_owned()))?;
    mac.update(signature_message(blob, expected, exp).as_bytes());
    mac.verify_slice(&sig)
        .map_err(|_| forbidden("invalid signed url signature"))?;
    if now_unix > exp {
        return Err(forbidden("signed url has expired"));
    }
    Ok(())
}

fn signature_message(blob: &BlobId, op: BlobOp, exp: u64) -> String {
    format!("blob:{blob}:{}:{exp}", op.as_str())
}

fn hmac_hex(signing_key: &SecretString, message: &[u8]) -> Result<String, CacheError> {
    let mut mac = HmacSha256::new_from_slice(signing_key.expose().as_bytes())
        .map_err(|_| CacheError::Internal("invalid hmac key".to_owned()))?;
    mac.update(message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_of(url: &str) -> (Option<String>, Option<u64>, Option<String>) {
        let query = url.split_once('?').map_or("", |(_, q)| q);
        let get = |name: &str| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix(&format!("{name}=")).map(str::to_owned))
        };
        (
            get("op"),
            get("exp").and_then(|e| e.parse().ok()),
            get("sig"),
        )
    }

    #[test]
    fn signed_urls_bind_blob_operation_and_expiry() -> Result<(), CacheError> {
        let key = SecretString::new("k".repeat(32));
        let blob = BlobId::generate();
        let url = sign_blob_url("http://cache:8090/", &key, &blob, BlobOp::Put, 100)?;
        assert!(url.starts_with(&format!("http://cache:8090/v1/cache/blobs/{blob}?")));
        let (op, exp, sig) = query_of(&url);
        let signed = SignedQuery {
            op: op.as_deref(),
            exp,
            sig: sig.as_deref(),
        };
        let verify =
            |key: &SecretString, blob: &BlobId, op: BlobOp, query: &SignedQuery<'_>, now| {
                verify_blob_url(key, blob, op, query, now).is_ok()
            };

        assert!(verify(&key, &blob, BlobOp::Put, &signed, 100));
        assert!(!verify(&key, &blob, BlobOp::Put, &signed, 101), "expired");
        assert!(!verify(
            &key,
            &blob,
            BlobOp::Get,
            &SignedQuery {
                op: Some("get"),
                ..signed
            },
            50
        ));
        assert!(
            !verify(&key, &BlobId::generate(), BlobOp::Put, &signed, 50),
            "other blob"
        );
        assert!(!verify(
            &key,
            &blob,
            BlobOp::Put,
            &SignedQuery {
                exp: Some(10_000),
                ..signed
            },
            50
        ));
        let other_key = SecretString::new("x".repeat(32));
        assert!(
            !verify(&other_key, &blob, BlobOp::Put, &signed, 50),
            "other key"
        );
        Ok(())
    }
}
