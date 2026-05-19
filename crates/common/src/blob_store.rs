//! Body-offload store for the audit pipeline.
//!
//! ## Why
//!
//! Captured request/response bodies (gateway prompts, completions, MCP
//! tool arguments/results) live in `gateway_logs.request_body` /
//! `response_body` / `mcp_logs.tool_arguments` / `tool_result` as
//! `Nullable(String)` columns inside ClickHouse. That works fine while
//! payloads stay small — the typical chat completion sits at 2-10 KB,
//! comfortably under the 256 KiB `audit.body_max_bytes` cap.
//!
//! It DOESN'T work for the long-tail scenarios that matter most to a
//! bastion's audit story: 200K-1M context prompts (long-document
//! analysis), MCP file-read tools returning full file contents,
//! multi-modal messages carrying base64-encoded images / audio, batch
//! requests with hundreds of message-history turns. For those the
//! body easily blows past inline; without offload we either truncate
//! (losing the evidence the bastion was deployed to preserve) or
//! force operators to raise the inline cap to absurd values (which
//! bloats CH row sizes, slows reads, and stresses inserts).
//!
//! This module abstracts the offload as a small `BlobStore` trait so
//! both modes ship in one shape:
//!
//! * [`InlineStore`] — never offloads. Used when no S3 endpoint is
//!   configured; bodies bigger than the inline cap fall back to
//!   truncation (the pre-P2 behavior).
//! * [`S3Store`] — uploads to any S3-compatible backend. Defaults
//!   target the bundled `rustfs` container in the dev compose so
//!   self-hosted deployments work out of the box; the same client
//!   talks to AWS S3 / MinIO / Ceph / etc. with no code change.
//!
//! The proxy.rs body-capture pipeline calls
//! [`BlobStore::store_if_oversize`] for every captured body; the
//! return value tells it whether to write the original string into
//! the audit row (inline) or an `s3://bucket/key` pointer that the
//! body-viewer endpoints dereference at read time.
//!
//! ## Crypto
//!
//! Signing reuses the workspace's existing `aws-sigv4` dep — same
//! pattern as `crates/gateway/src/providers/bedrock.rs`. No
//! `aws-sdk-s3` import (the SDK adds ~50 transitive crates for what's
//! effectively GET/PUT for our case), no new TLS stack.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("blob store not configured (set S3_BUCKET + S3_ENDPOINT_URL to enable)")]
    NotConfigured,
    #[error("S3 SigV4 signing failed: {0}")]
    Signing(String),
    #[error("S3 HTTP transport failed: {0}")]
    Transport(String),
    #[error("S3 returned HTTP {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("malformed s3:// URL: {0}")]
    BadUrl(String),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Result of `store_if_oversize` — either we kept the body inline, or
/// we uploaded it and the caller should persist the returned URL.
#[derive(Debug, Clone)]
pub enum BlobDecision {
    /// Body fit inline (or store has `can_offload() == false`). The
    /// `String` is the original content; caller writes it into the
    /// audit row's body column verbatim.
    Inline(String),
    /// Body was uploaded. `url` is `s3://bucket/key` shape; caller
    /// writes the URL into the body column and records the original
    /// `bytes` count in the `*_body_bytes` column.
    Offloaded { url: String, bytes: usize },
}

/// `(table, log_id, field)` triple used to construct a deterministic
/// S3 object key. Kept as a single struct so the trait signature
/// doesn't grow positional `&str` parameters that callers can swap.
#[derive(Debug, Clone, Copy)]
pub struct BlobKeyHint<'a> {
    pub table: &'a str,
    pub log_id: &'a str,
    pub field: &'a str,
}

#[async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Whether this store can actually offload. `InlineStore` returns
    /// `false`; an `S3Store` returns `true` once configured.
    fn can_offload(&self) -> bool;

    /// Decide inline vs offload for one body. The caller has already
    /// computed `content.len()` and knows the `inline_threshold` — we
    /// pass both in so the policy lives at the call site (proxy.rs)
    /// while the offload mechanics stay here. Implementations that
    /// can't offload should always return [`BlobDecision::Inline`].
    async fn store_if_oversize(
        &self,
        hint: BlobKeyHint<'_>,
        content: String,
        inline_threshold: usize,
    ) -> Result<BlobDecision, BlobError>;

    /// Dereference a stored URL (s3://bucket/key) back to raw bytes.
    /// Used by the body-viewer endpoints to render the original body
    /// when the audit row points at remote storage.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, BlobError>;
}

// ---------------------------------------------------------------------------
// InlineStore — no-op fallback when S3 isn't configured
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct InlineStore;

#[async_trait]
impl BlobStore for InlineStore {
    fn can_offload(&self) -> bool {
        false
    }
    async fn store_if_oversize(
        &self,
        _hint: BlobKeyHint<'_>,
        content: String,
        _inline_threshold: usize,
    ) -> Result<BlobDecision, BlobError> {
        // No offload available. Caller is responsible for truncating
        // when `content.len()` exceeds `audit.body_max_bytes`; we
        // pass the string through unchanged because we have no
        // remote storage to fall back on.
        Ok(BlobDecision::Inline(content))
    }
    async fn fetch(&self, _url: &str) -> Result<Vec<u8>, BlobError> {
        // The audit row should never contain an s3:// URL when this
        // store is wired in — but if a deployment was reconfigured
        // from S3 → inline mid-flight, an old row can still carry
        // one. Surface the misconfig cleanly instead of panicking.
        Err(BlobError::NotConfigured)
    }
}

// ---------------------------------------------------------------------------
// S3Store — single backend, fits AWS S3 / RustFS / MinIO / Ceph
// ---------------------------------------------------------------------------

/// Configuration knobs read from env at server start. `endpoint` is
/// mandatory when offload is enabled; the default dev-compose ships
/// `http://rustfs:9000`. `path_style` defaults `true` for
/// self-hosted compatibility (MinIO / RustFS) and `false` for AWS
/// (virtual-host-style is required there for new buckets).
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub path_style: bool,
}

#[derive(Debug)]
pub struct S3Store {
    cfg: S3Config,
    http: reqwest::Client,
}

impl S3Store {
    pub fn new(cfg: S3Config, http: reqwest::Client) -> Arc<Self> {
        Arc::new(Self { cfg, http })
    }

    /// Build the canonical object URL for a key. Path-style returns
    /// `https://<endpoint>/<bucket>/<key>` (MinIO/RustFS default);
    /// virtual-host-style returns `https://<bucket>.<endpoint-host>/<key>`
    /// (AWS S3 default). The choice is config-driven because both
    /// backends reject the wrong shape with confusing 403s.
    fn object_url(&self, key: &str) -> Result<String, BlobError> {
        let endpoint = self.cfg.endpoint.trim_end_matches('/');
        if self.cfg.path_style {
            Ok(format!("{endpoint}/{}/{}", self.cfg.bucket, key))
        } else {
            // Replace scheme://host with scheme://bucket.host
            let parsed = url::Url::parse(endpoint)
                .map_err(|e| BlobError::BadUrl(format!("S3 endpoint: {e}")))?;
            let host = parsed.host_str().ok_or_else(|| {
                BlobError::BadUrl("S3 endpoint missing host for virtual-host style".to_string())
            })?;
            let scheme = parsed.scheme();
            let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
            Ok(format!("{scheme}://{}.{host}{port}/{key}", self.cfg.bucket))
        }
    }

    /// Build the stable `s3://bucket/key` URL we store in the audit row.
    /// Independent of `path_style` because the persisted URL needs to
    /// survive a switch between AWS-style and MinIO-style deployments.
    fn stored_url(&self, key: &str) -> String {
        format!("s3://{}/{}", self.cfg.bucket, key)
    }

    /// Parse `s3://bucket/key` and confirm the bucket matches. Other
    /// bucket names indicate the audit row was written by a deployment
    /// pointing at a different bucket; we refuse to fetch from there
    /// rather than silently pulling from somewhere the operator may
    /// not have access to.
    fn parse_s3_url<'a>(&self, url: &'a str) -> Result<&'a str, BlobError> {
        let key = url
            .strip_prefix("s3://")
            .ok_or_else(|| BlobError::BadUrl(format!("not an s3:// URL: {url}")))?;
        let (bucket, key_part) = key
            .split_once('/')
            .ok_or_else(|| BlobError::BadUrl(format!("missing key path: {url}")))?;
        if bucket != self.cfg.bucket {
            return Err(BlobError::BadUrl(format!(
                "refusing cross-bucket fetch — stored URL bucket {bucket:?} != configured {:?}",
                self.cfg.bucket
            )));
        }
        Ok(key_part)
    }

    /// SigV4-sign + send one request. `method` and `body` map to the
    /// HTTP verb + payload; the function returns the response body
    /// bytes on 2xx, or [`BlobError::BadStatus`] otherwise.
    async fn signed_request(
        &self,
        method: &'static str,
        url: &str,
        body: &[u8],
    ) -> Result<bytes::Bytes, BlobError> {
        let credentials = Credentials::new(
            &self.cfg.access_key_id,
            &self.cfg.secret_access_key,
            None,
            None,
            "think-watch",
        );
        let identity = credentials.into();
        let mut signing_settings = SigningSettings::default();
        signing_settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.cfg.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| BlobError::Signing(e.to_string()))?;

        let signable_request =
            SignableRequest::new(method, url, std::iter::empty(), SignableBody::Bytes(body))
                .map_err(|e| BlobError::Signing(e.to_string()))?;

        let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
            .map_err(|e| BlobError::Signing(e.to_string()))?
            .into_parts();

        let mut http_req = http_1x::Request::builder()
            .method(method)
            .uri(url)
            .body(())
            .map_err(|e| BlobError::Signing(format!("http builder: {e}")))?;
        signing_instructions.apply_to_request_http1x(&mut http_req);

        let mut req_builder = match method {
            "PUT" => self.http.put(url),
            "GET" => self.http.get(url),
            "DELETE" => self.http.delete(url),
            other => return Err(BlobError::Signing(format!("unsupported method: {other}"))),
        };
        for (name, value) in http_req.headers().iter() {
            let n = name.as_str();
            // Forward AWS-signed headers only. SigV4 covers
            // `authorization` + every `x-amz-*` header it added; the
            // empty header iter above means SignableRequest didn't
            // have to sign any operator-controlled values.
            if (n == "authorization" || n.starts_with("x-amz-"))
                && let Ok(v) = value.to_str()
            {
                req_builder = req_builder.header(n, v);
            }
        }
        if !body.is_empty() {
            req_builder = req_builder.body(body.to_vec());
        }

        let resp = req_builder
            .send()
            .await
            .map_err(|e| BlobError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BlobError::Transport(e.to_string()))?;
        if !status.is_success() {
            // Body of an S3 error is XML — return up to 1 KB so
            // operators can read the `<Code>` and `<Message>`
            // diagnostic without flooding logs with multi-MB
            // payloads on a misbehaving backend.
            let mut snippet = String::from_utf8_lossy(&bytes).to_string();
            const MAX: usize = 1024;
            if snippet.len() > MAX {
                snippet.truncate(MAX);
                snippet.push_str("...");
            }
            return Err(BlobError::BadStatus {
                status: status.as_u16(),
                body: snippet,
            });
        }
        Ok(bytes)
    }
}

#[async_trait]
impl BlobStore for S3Store {
    fn can_offload(&self) -> bool {
        true
    }

    async fn store_if_oversize(
        &self,
        hint: BlobKeyHint<'_>,
        content: String,
        inline_threshold: usize,
    ) -> Result<BlobDecision, BlobError> {
        if content.len() <= inline_threshold {
            return Ok(BlobDecision::Inline(content));
        }
        // Object key shape:
        //   bodies/<table>/<yyyy>/<mm>/<dd>/<log_id>-<field>.json
        // The date prefix gives operators a cheap S3 lifecycle hook
        // (Expire after N days, prefix = bodies/<table>/<old-date>)
        // and matches how analysts navigate the bucket manually.
        let now = chrono::Utc::now();
        let key = format!(
            "bodies/{}/{}/{:02}/{:02}/{}-{}.json",
            hint.table,
            now.format("%Y"),
            now.month(),
            now.day(),
            hint.log_id,
            hint.field
        );
        let bytes = content.len();
        let object_url = self.object_url(&key)?;
        self.signed_request("PUT", &object_url, content.as_bytes())
            .await?;
        metrics::counter!("blob_store_put_total", "field" => hint.field.to_string()).increment(1);
        metrics::counter!("blob_store_put_bytes_total", "field" => hint.field.to_string())
            .increment(bytes as u64);
        Ok(BlobDecision::Offloaded {
            url: self.stored_url(&key),
            bytes,
        })
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>, BlobError> {
        let key = self.parse_s3_url(url)?;
        let object_url = self.object_url(key)?;
        let bytes = self.signed_request("GET", &object_url, b"").await?;
        metrics::counter!("blob_store_get_total").increment(1);
        Ok(bytes.to_vec())
    }
}

// chrono Datelike is needed for month()/day() in the S3 key formatter.
use chrono::Datelike;

// ---------------------------------------------------------------------------
// Construction helper
// ---------------------------------------------------------------------------

/// Build the configured store at startup. Reads env vars; falls back
/// to [`InlineStore`] when no bucket is configured (so the build /
/// dev path works without RustFS).
pub fn build_from_env(http: reqwest::Client) -> Arc<dyn BlobStore> {
    let bucket = match std::env::var("S3_BUCKET") {
        Ok(b) if !b.is_empty() => b,
        _ => {
            tracing::info!(
                "S3_BUCKET unset — audit body offload disabled, oversized bodies will be \
                 truncated to audit.body_max_bytes"
            );
            return Arc::new(InlineStore);
        }
    };
    let endpoint = std::env::var("S3_ENDPOINT_URL").unwrap_or_else(|_| {
        // Reasonable fallback for the bundled rustfs container.
        "http://rustfs:9000".to_string()
    });
    let region = std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let access_key_id = std::env::var("S3_ACCESS_KEY_ID").unwrap_or_default();
    let secret_access_key = std::env::var("S3_SECRET_ACCESS_KEY").unwrap_or_default();
    let path_style = std::env::var("S3_PATH_STYLE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(true);

    if access_key_id.is_empty() || secret_access_key.is_empty() {
        tracing::error!(
            "S3_BUCKET set but S3_ACCESS_KEY_ID / S3_SECRET_ACCESS_KEY missing — \
             audit body offload disabled, set both to enable"
        );
        return Arc::new(InlineStore);
    }

    let cfg = S3Config {
        endpoint,
        region,
        bucket,
        access_key_id,
        secret_access_key,
        path_style,
    };
    tracing::info!(
        bucket = cfg.bucket,
        endpoint = cfg.endpoint,
        path_style,
        "audit body offload enabled (S3-compatible backend)"
    );
    S3Store::new(cfg, http) as Arc<dyn BlobStore>
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inline_store_keeps_content_inline() {
        let store = InlineStore;
        assert!(!store.can_offload());
        let hint = BlobKeyHint {
            table: "gateway_logs",
            log_id: "abc",
            field: "request",
        };
        let result = store
            .store_if_oversize(hint, "small body".to_string(), 100)
            .await
            .unwrap();
        match result {
            BlobDecision::Inline(s) => assert_eq!(s, "small body"),
            _ => panic!("inline store must never offload"),
        }
    }

    #[tokio::test]
    async fn inline_store_never_offloads_even_when_oversized() {
        // The truncation-vs-offload decision lives at the caller. When
        // there's no store to offload to, the caller must handle the
        // oversize case themselves (truncate). The store just passes
        // the content through.
        let store = InlineStore;
        let hint = BlobKeyHint {
            table: "x",
            log_id: "y",
            field: "z",
        };
        let big = "x".repeat(10_000);
        let result = store
            .store_if_oversize(hint, big.clone(), 100)
            .await
            .unwrap();
        assert!(matches!(result, BlobDecision::Inline(s) if s.len() == 10_000));
    }

    #[tokio::test]
    async fn inline_store_fetch_returns_not_configured() {
        let store = InlineStore;
        let err = store.fetch("s3://bucket/key").await.unwrap_err();
        assert!(matches!(err, BlobError::NotConfigured));
    }

    fn cfg(path_style: bool) -> S3Config {
        S3Config {
            endpoint: "https://s3.example.com".to_string(),
            region: "us-east-1".to_string(),
            bucket: "audit-bodies".to_string(),
            access_key_id: "AKIA".to_string(),
            secret_access_key: "secret".to_string(),
            path_style,
        }
    }

    #[test]
    fn object_url_path_style() {
        let s = S3Store {
            cfg: cfg(true),
            http: reqwest::Client::new(),
        };
        assert_eq!(
            s.object_url("bodies/gateway_logs/2026/05/18/abc-request.json")
                .unwrap(),
            "https://s3.example.com/audit-bodies/bodies/gateway_logs/2026/05/18/abc-request.json"
        );
    }

    #[test]
    fn object_url_virtual_host_style() {
        let s = S3Store {
            cfg: cfg(false),
            http: reqwest::Client::new(),
        };
        assert_eq!(
            s.object_url("bodies/x.json").unwrap(),
            "https://audit-bodies.s3.example.com/bodies/x.json"
        );
    }

    #[test]
    fn stored_url_is_bucket_scoped() {
        let s = S3Store {
            cfg: cfg(true),
            http: reqwest::Client::new(),
        };
        assert_eq!(
            s.stored_url("bodies/x.json"),
            "s3://audit-bodies/bodies/x.json"
        );
    }

    #[test]
    fn parse_s3_url_extracts_key() {
        let s = S3Store {
            cfg: cfg(true),
            http: reqwest::Client::new(),
        };
        assert_eq!(
            s.parse_s3_url("s3://audit-bodies/bodies/x.json").unwrap(),
            "bodies/x.json"
        );
    }

    #[test]
    fn parse_s3_url_refuses_cross_bucket() {
        let s = S3Store {
            cfg: cfg(true),
            http: reqwest::Client::new(),
        };
        let err = s.parse_s3_url("s3://other-bucket/x.json").unwrap_err();
        assert!(matches!(err, BlobError::BadUrl(_)));
    }

    #[test]
    fn parse_s3_url_rejects_missing_key() {
        let s = S3Store {
            cfg: cfg(true),
            http: reqwest::Client::new(),
        };
        let err = s.parse_s3_url("s3://audit-bodies").unwrap_err();
        assert!(matches!(err, BlobError::BadUrl(_)));
    }
}
