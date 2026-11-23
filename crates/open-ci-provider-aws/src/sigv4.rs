//! AWS Signature Version 4 (`AWS4-HMAC-SHA256`) request signer.
//!
//! Enables pure-Rust EC2 and STS Query API calls over `reqwest` without
//! shelling out to the Python `aws` CLI or pulling in heavy generated SDKs.

use hmac::{Hmac, Mac};
use open_ci_core::SecretString;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;

/// AWS IAM credentials used to sign requests.
#[derive(Clone, Debug)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
    pub session_token: Option<SecretString>,
}

/// Signed headers produced for an AWS HTTP request.
#[derive(Clone, Debug)]
pub struct SignedHeaders {
    pub amz_date: String,
    pub payload_sha256: String,
    pub authorization: String,
    pub security_token: Option<SecretString>,
}

/// Signs an HTTP `POST` Query API request using AWS Signature Version 4.
pub fn sign_post(
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    host: &str,
    path: &str,
    body: &str,
    now_unix: u64,
) -> Result<SignedHeaders, String> {
    let dt = OffsetDateTime::from_unix_timestamp(
        i64::try_from(now_unix).map_err(|_| "timestamp overflow".to_owned())?,
    )
    .map_err(|_| "invalid unix timestamp".to_owned())?;

    let date_stamp = format!("{:04}{:02}{:02}", dt.year(), u8::from(dt.month()), dt.day());
    let amz_date = format!(
        "{date_stamp}T{:02}{:02}{:02}Z",
        dt.hour(),
        dt.minute(),
        dt.second()
    );

    let payload_sha256 = sha256_hex(body.as_bytes());
    let canonical_uri = if path.is_empty() { "/" } else { path };
    let host_lower = host.to_ascii_lowercase();

    let (canonical_headers, signed_headers) = if let Some(token) = &creds.session_token {
        (
            format!(
                "content-type:application/x-www-form-urlencoded; charset=utf-8\nhost:{host_lower}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\nx-amz-security-token:{}\n",
                token.expose()
            ),
            "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token",
        )
    } else {
        (
            format!(
                "content-type:application/x-www-form-urlencoded; charset=utf-8\nhost:{host_lower}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\n"
            ),
            "content-type;host;x-amz-content-sha256;x-amz-date",
        )
    };

    let canonical_request =
        format!("POST\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_sha256}");

    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key.expose()).as_bytes(),
        date_stamp.as_bytes(),
    )?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes())?);

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );

    Ok(SignedHeaders {
        amz_date,
        payload_sha256,
        authorization,
        security_token: creds.session_token.clone(),
    })
}

/// Percent-encodes a query parameter according to RFC 3986 (required by AWS Query API).
pub fn form_encode(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            let upper = hex::encode_upper([byte]);
            out.push_str(&upper);
        }
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| "invalid hmac key length".to_owned())?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_deterministic_aws4_authorization_header() {
        let creds = AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            session_token: None,
        };
        let signed = sign_post(
            &creds,
            "us-east-1",
            "ec2",
            "ec2.us-east-1.amazonaws.com",
            "/",
            "Action=DescribeInstances&Version=2016-11-15",
            1_700_000_000,
        );
        assert!(signed.is_ok());
        if let Ok(headers) = signed {
            assert!(headers.authorization.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20231114/us-east-1/ec2/aws4_request"
            ));
            assert_eq!(headers.amz_date, "20231114T221320Z");
        }
    }
}
