//! Loopback OIDC issuer that serves a real JWKS and mints RS256 Actions runtime tokens for tests.

use axum::Router;
use axum::routing::get;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TEST_ONLY_SIGNING_KEY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/actions-runtime-token-TEST-ONLY.pem"
));
const KEY_ID: &str = "open-ci-test-key";

#[derive(Debug)]
pub struct TestIssuer {
    issuer: String,
    key: EncodingKey,
    server: JoinHandle<()>,
}

impl TestIssuer {
    pub async fn start() -> io::Result<Self> {
        let key = EncodingKey::from_rsa_pem(TEST_ONLY_SIGNING_KEY.as_bytes())
            .map_err(io::Error::other)?;
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::RS256).map_err(io::Error::other)?;
        jwk.common.key_id = Some(KEY_ID.to_owned());
        let jwks = JwkSet { keys: vec![jwk] };

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let issuer = format!("http://{}", listener.local_addr()?);
        let app = Router::new().route(
            "/.well-known/jwks",
            get(move || {
                let jwks = jwks.clone();
                async move { axum::Json(jwks) }
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            issuer,
            key,
            server,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Signs arbitrary claims with the issuer's key and key ID.
    pub fn sign(&self, claims: &Value) -> Result<String, jsonwebtoken::errors::Error> {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KEY_ID.to_owned());
        encode(&header, claims, &self.key)
    }

    /// Mints a runtime token shaped like GitHub's: `ac` grants as a JSON string, string repo ID.
    pub fn runtime_token(
        &self,
        repository_id: u64,
        grants: &[(&str, u32)],
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let ac: Vec<Value> = grants
            .iter()
            .map(|(scope, permission)| json!({"Scope": scope, "Permission": permission}))
            .collect();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.sign(&json!({
            "iss": self.issuer,
            "exp": now + 600,
            "nbf": now.saturating_sub(5),
            "ac": Value::Array(ac).to_string(),
            "repository_id": repository_id.to_string(),
        }))
    }
}

impl Drop for TestIssuer {
    fn drop(&mut self) {
        self.server.abort();
    }
}
