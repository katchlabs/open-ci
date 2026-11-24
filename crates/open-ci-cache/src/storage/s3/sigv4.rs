//! AWS Signature Version 4 for S3 requests with arbitrary method, path, query, and headers.

use crate::error::CacheError;
use hmac::{Hmac, Mac};
use open_ci_provider_aws::sigv4::AwsCredentials;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "s3";
pub(super) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub(super) const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Request timestamp in the two forms `SigV4` needs: `x-amz-date` and the credential-scope date.
#[derive(Debug)]
pub(super) struct Timestamp {
    amz_date: String,
    date_stamp: String,
}

impl Timestamp {
    pub(super) fn from_unix(now_unix: u64) -> Result<Self, CacheError> {
        let dt = i64::try_from(now_unix)
            .ok()
            .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok())
            .ok_or_else(|| CacheError::Internal("clock outside signable range".to_owned()))?;
        let date_stamp = format!("{:04}{:02}{:02}", dt.year(), u8::from(dt.month()), dt.day());
        Ok(Self {
            amz_date: format!(
                "{date_stamp}T{:02}{:02}{:02}Z",
                dt.hour(),
                dt.minute(),
                dt.second()
            ),
            date_stamp,
        })
    }

    pub(super) fn amz_date(&self) -> &str {
        &self.amz_date
    }

    fn date_stamp(&self) -> &str {
        &self.date_stamp
    }
}

/// Everything that is covered by the signature. `headers` must already contain `host`,
/// `x-amz-date`, `x-amz-content-sha256`, and any `x-amz-security-token`, with lowercase names.
#[derive(Debug)]
pub(super) struct CanonicalRequest<'a> {
    pub(super) method: &'a str,
    pub(super) canonical_uri: &'a str,
    pub(super) canonical_query: &'a str,
    pub(super) headers: &'a [(String, String)],
    pub(super) payload_hash: &'a str,
}

impl CanonicalRequest<'_> {
    fn render(&self) -> (String, String) {
        let mut headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.trim()))
            .collect();
        headers.sort_unstable();
        let canonical_headers = headers
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<Vec<_>>()
            .concat();
        let signed_headers = headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");
        let rendered = format!(
            "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
            self.method, self.canonical_uri, self.canonical_query, self.payload_hash
        );
        (rendered, signed_headers)
    }
}

/// Computes the `Authorization` header value for `request`.
pub(super) fn authorization(
    credentials: &AwsCredentials,
    region: &str,
    timestamp: &Timestamp,
    request: &CanonicalRequest<'_>,
) -> Result<String, CacheError> {
    let (canonical_request, signed_headers) = request.render();
    let scope = format!("{}/{region}/{SERVICE}/aws4_request", timestamp.date_stamp());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        timestamp.amz_date(),
        sha256_hex(canonical_request.as_bytes())
    );
    let secret = format!("AWS4{}", credentials.secret_access_key.expose());
    let mut key = hmac(secret.as_bytes(), timestamp.date_stamp().as_bytes())?;
    for part in [region, SERVICE, "aws4_request"] {
        key = hmac(&key, part.as_bytes())?;
    }
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes())?);
    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    ))
}

/// RFC 3986 percent-encoding as S3 canonicalizes it: unreserved bytes pass through and `/` is
/// kept only for object-key paths, never inside query components.
pub(super) fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&hex::encode_upper([byte]));
        }
    }
    out
}

/// Sorted, encoded query string; doubles as the literal query sent on the wire.
pub(super) fn canonical_query(params: &[(&str, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| (uri_encode(name, false), uri_encode(value, false)))
        .collect();
    encoded.sort_unstable();
    encoded
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

pub(super) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>, CacheError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| CacheError::Internal("invalid hmac key".to_owned()))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_ci_core::SecretString;

    fn example_credentials() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            session_token: None,
        }
    }

    fn example_headers(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            (
                "x-amz-content-sha256".to_owned(),
                EMPTY_PAYLOAD_SHA256.to_owned(),
            ),
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
        ];
        headers.extend(
            extra
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        headers
    }

    fn signature_of(authorization: &str) -> Option<&str> {
        authorization.rsplit_once("Signature=").map(|(_, sig)| sig)
    }

    #[test]
    fn reproduces_aws_documented_get_object_signature() -> Result<(), CacheError> {
        let timestamp = Timestamp::from_unix(1_369_353_600)?;
        assert_eq!(timestamp.amz_date(), "20130524T000000Z");
        let headers = example_headers(&[("range", "bytes=0-9")]);
        let request = CanonicalRequest {
            method: "GET",
            canonical_uri: &format!("/{}", uri_encode("test.txt", true)),
            canonical_query: "",
            headers: &headers,
            payload_hash: EMPTY_PAYLOAD_SHA256,
        };
        assert_eq!(
            sha256_hex(request.render().0.as_bytes()),
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
        let auth = authorization(&example_credentials(), "us-east-1", &timestamp, &request)?;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        Ok(())
    }

    #[test]
    fn reproduces_aws_documented_list_objects_signature() -> Result<(), CacheError> {
        let timestamp = Timestamp::from_unix(1_369_353_600)?;
        let headers = example_headers(&[]);
        let query = canonical_query(&[("prefix", "J".to_owned()), ("max-keys", "2".to_owned())]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let request = CanonicalRequest {
            method: "GET",
            canonical_uri: "/",
            canonical_query: &query,
            headers: &headers,
            payload_hash: EMPTY_PAYLOAD_SHA256,
        };
        let auth = authorization(&example_credentials(), "us-east-1", &timestamp, &request)?;
        assert_eq!(
            signature_of(&auth),
            Some("34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7")
        );
        Ok(())
    }

    #[test]
    fn encodes_keys_and_query_values_like_s3() {
        assert_eq!(
            uri_encode("ci cache+v1/blobs/a~b", true),
            "ci%20cache%2Bv1/blobs/a~b"
        );
        assert_eq!(uri_encode("a/b=c&d", false), "a%2Fb%3Dc%26d");
        assert_eq!(
            canonical_query(&[
                ("uploadId", "x/y+z".to_owned()),
                ("partNumber", "10".to_owned()),
                ("uploads", String::new()),
            ]),
            "partNumber=10&uploadId=x%2Fy%2Bz&uploads="
        );
    }
}
