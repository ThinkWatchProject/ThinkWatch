//! HTTP client used by integration tests.
//!
//! Why not just use `reqwest::Client` with the cookies feature?
//! Because the server sets `__Host-` / `__Secure-` cookies that
//! reqwest's jar refuses to send back over plain `http://`. We
//! manage cookies by hand instead — explicit, portable, and exposes
//! the raw `Set-Cookie` headers to the assertions that need them.
//!
//! Signing — when [`SignedKey`] is attached, every mutating request
//! (POST/PUT/PATCH/DELETE) gets the three signature headers the
//! `verify_signature` middleware expects, computed against the same
//! string-to-sign the production frontend uses:
//! `{METHOD}\n{PATH_AND_QUERY}\n{TIMESTAMP}\n{NONCE}\n{SHA256(BODY)}`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::pkcs8::EncodePublicKey;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct TestClient {
    base: String,
    inner: Client,
    cookies: Arc<Mutex<HashMap<String, String>>>,
    signing: Arc<Mutex<Option<SignedKey>>>,
    bearer: Arc<Mutex<Option<String>>>,
    /// Optional `X-Forwarded-For` value. Tests that mess with rate
    /// limits flip this so the gateway sees a different IP. Honoured
    /// only when `security.trusted_proxies` is configured to trust
    /// the test loopback — by default the server falls back to the
    /// connection IP (also 127.0.0.1) and ignores the header.
    forwarded_for: Arc<Mutex<Option<String>>>,
}

impl TestClient {
    pub fn new(base: impl Into<String>) -> Self {
        let inner = Client::builder()
            // Don't follow redirects automatically — tests assert on
            // 30x bodies for SSO callbacks.
            .redirect(reqwest::redirect::Policy::none())
            // Be patient: in-process server + Argon2 password hashing
            // can run >5s on a cold cache when CI is loaded.
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        Self {
            base: base.into(),
            inner,
            cookies: Arc::new(Mutex::new(HashMap::new())),
            signing: Arc::new(Mutex::new(None)),
            bearer: Arc::new(Mutex::new(None)),
            forwarded_for: Arc::new(Mutex::new(None)),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn set_bearer(&self, token: impl Into<String>) {
        *self.bearer.lock().unwrap() = Some(token.into());
    }

    pub fn clear_bearer(&self) {
        *self.bearer.lock().unwrap() = None;
    }

    pub fn set_forwarded_for(&self, ip: impl Into<String>) {
        *self.forwarded_for.lock().unwrap() = Some(ip.into());
    }

    /// Get the value of a stored cookie. Useful for assertions like
    /// "logout cleared __Host-access_token".
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies.lock().unwrap().get(name).cloned()
    }

    /// Replace the cookie store from a raw `Set-Cookie` value list,
    /// e.g. when the test wants to simulate a stale browser.
    pub fn set_cookie(&self, name: impl Into<String>, value: impl Into<String>) {
        self.cookies
            .lock()
            .unwrap()
            .insert(name.into(), value.into());
    }

    pub fn clear_cookies(&self) {
        self.cookies.lock().unwrap().clear();
    }

    /// Generate a fresh ECDSA P-256 keypair, attach it for signing
    /// future mutating requests, and return the JWK ready for
    /// `POST /api/auth/register-key`. The caller is expected to ship
    /// that JWK to the server before issuing more signed requests.
    pub fn enable_signing(&self) -> Value {
        let signing = SignedKey::generate();
        let jwk = signing.public_jwk();
        *self.signing.lock().unwrap() = Some(signing);
        jwk
    }

    pub fn disable_signing(&self) {
        *self.signing.lock().unwrap() = None;
    }

    /// Swap in a previously-generated signing key. Tests use this to
    /// pin rotation behaviour: register key A, register key B,
    /// restore the client to key A, and assert that A-signed requests
    /// now fail because Redis has B's pubkey.
    pub fn set_signing_key(&self, key: SignedKey) {
        *self.signing.lock().unwrap() = Some(key);
    }

    // ---- request methods --------------------------------------------------

    pub async fn get(&self, path: &str) -> Result<TestResponse> {
        self.send(Method::GET, path, None::<&Value>).await
    }

    pub async fn post(&self, path: &str, body: impl Serialize) -> Result<TestResponse> {
        self.send(Method::POST, path, Some(&body)).await
    }

    pub async fn post_empty(&self, path: &str) -> Result<TestResponse> {
        self.send(Method::POST, path, Some(&serde_json::json!({})))
            .await
    }

    pub async fn patch(&self, path: &str, body: impl Serialize) -> Result<TestResponse> {
        self.send(Method::PATCH, path, Some(&body)).await
    }

    pub async fn put(&self, path: &str, body: impl Serialize) -> Result<TestResponse> {
        self.send(Method::PUT, path, Some(&body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<TestResponse> {
        self.send(Method::DELETE, path, None::<&Value>).await
    }

    pub async fn delete_with_body(&self, path: &str, body: impl Serialize) -> Result<TestResponse> {
        self.send(Method::DELETE, path, Some(&body)).await
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<TestResponse> {
        let url = format!("{}{}", self.base, path);
        let body_bytes = match body {
            Some(b) => Some(serde_json::to_vec(b).context("serialize body")?),
            None => None,
        };

        // Build request. Don't go through reqwest's body serialiser
        // for JSON because we already have the bytes (needed for
        // SHA-256 hashing).
        let mut req = self.inner.request(method.clone(), &url);
        if let Some(bytes) = &body_bytes {
            req = req
                .header("content-type", "application/json")
                .body(bytes.clone());
        }
        req = self.attach_cookies(req);
        if let Some(token) = self.bearer.lock().unwrap().clone() {
            req = req.bearer_auth(token);
        }
        if let Some(xff) = self.forwarded_for.lock().unwrap().clone() {
            req = req.header("x-forwarded-for", xff);
        }
        req = self.maybe_sign(req, &method, path, body_bytes.as_deref());

        let resp = req
            .send()
            .await
            .with_context(|| format!("HTTP {method} {url}"))?;
        self.absorb_set_cookies(&resp);
        TestResponse::from_response(resp).await
    }

    fn attach_cookies(&self, mut req: RequestBuilder) -> RequestBuilder {
        let cookies = self.cookies.lock().unwrap();
        if !cookies.is_empty() {
            let header = cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            req = req.header("cookie", header);
        }
        req
    }

    fn maybe_sign(
        &self,
        mut req: RequestBuilder,
        method: &Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> RequestBuilder {
        // The middleware skips GET/HEAD/OPTIONS, so we do too.
        if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
            return req;
        }
        let Some(key) = self.signing.lock().unwrap().clone() else {
            return req;
        };
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let body_hash = hex::encode(Sha256::digest(body.unwrap_or_default()));
        let string_to_sign = format!("{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}");
        let sig: Signature = key.signing.sign(string_to_sign.as_bytes());
        let sig_b64 = data_encoding::BASE64URL_NOPAD.encode(&sig.to_bytes());
        req = req
            .header("x-signature-timestamp", timestamp)
            .header("x-signature-nonce", nonce)
            .header("x-signature", format!("ecdsa-p256:{sig_b64}"));
        req
    }

    fn absorb_set_cookies(&self, resp: &Response) {
        for raw in resp.headers().get_all("set-cookie") {
            let Ok(value) = raw.to_str() else { continue };
            let Some((kv, attrs)) = value.split_once(';').or(Some((value, ""))) else {
                continue;
            };
            let Some((name, val)) = kv.split_once('=') else {
                continue;
            };
            let name = name.trim().to_string();
            let val = val.trim().to_string();
            // `Max-Age=0` (or empty value) → cookie cleared.
            let cleared = val.is_empty()
                || attrs
                    .to_ascii_lowercase()
                    .split(';')
                    .map(|s| s.trim())
                    .any(|s| s == "max-age=0");
            let mut cookies = self.cookies.lock().unwrap();
            if cleared {
                cookies.remove(&name);
            } else {
                cookies.insert(name, val);
            }
        }
    }
}

pub struct TestResponse {
    pub status: reqwest::StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: bytes::Bytes,
}

impl TestResponse {
    async fn from_response(resp: Response) -> Result<Self> {
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.context("read response body")?;
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).with_context(|| {
            format!(
                "decode JSON response (status={}, body={:?})",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn assert_status(&self, expected: u16) -> &Self {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "expected HTTP {expected}, got {} — body: {}",
            self.status,
            self.text()
        );
        self
    }

    pub fn assert_ok(&self) -> &Self {
        assert!(
            self.status.is_success(),
            "expected 2xx, got {} — body: {}",
            self.status,
            self.text()
        );
        self
    }
}

#[derive(Clone)]
pub struct SignedKey {
    pub signing: SigningKey,
}

impl SignedKey {
    pub fn generate() -> Self {
        // Loop until `from_slice` accepts the random bytes (a private
        // scalar of zero or ≥ curve order is rejected; both are
        // vanishingly rare).
        loop {
            let mut bytes = [0u8; 32];
            rand::fill(&mut bytes[..]);
            if let Ok(signing) = SigningKey::from_slice(&bytes) {
                return Self { signing };
            }
        }
    }

    /// Public key as the JWK shape the `register-key` handler expects.
    pub fn public_jwk(&self) -> Value {
        let public = self.signing.verifying_key();
        // `to_public_key_der` → SPKI; we want the raw EC point. p256
        // exposes `to_jwk_string` on `PublicKey` directly.
        let pk: p256::PublicKey = public.into();
        let jwk_str = pk.to_jwk_string();
        let jwk: Value = serde_json::from_str(&jwk_str).expect("p256 jwk valid JSON");
        jwk
    }

    /// SPKI DER — handy if a future test wants to verify the key
    /// shape end-to-end.
    pub fn spki_der(&self) -> Vec<u8> {
        let pk: p256::PublicKey = self.signing.verifying_key().into();
        pk.to_public_key_der()
            .expect("encode SPKI")
            .as_bytes()
            .to_vec()
    }
}
