//! Per-transport delivery functions: UDP/TCP syslog, Kafka REST proxy,
//! webhook. Plus the [`ForwarderRuntime`] state struct each delivery
//! reads from, and the HMAC helper that signs webhook payloads.
//!
//! These functions stay free of business logic — they receive a
//! pre-built [`AuditEntry`] and a runtime/config, and return
//! `Result<(), String>` so both the inline path ([`forward_to_all`])
//! and the durable-retry path ([`drain_once`]) can dispatch by
//! `forwarder_type` against the same set.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::types::AuditEntry;
use crate::models::LogForwarder;

// ---------------------------------------------------------------------------
// Forwarder runtime state — one per active forwarder row
// ---------------------------------------------------------------------------

pub(super) struct ForwarderRuntime {
    pub(super) config: LogForwarder,
    pub(super) udp_socket: Option<UdpSocket>,
    pub(super) tcp_stream: Arc<Mutex<Option<tokio::net::TcpStream>>>,
}

/// Shared forwarder registry, reloaded periodically from the database,
/// and the URL check every delivery goes through.
pub(super) type ForwarderRegistry = Arc<Registry>;

pub(super) struct Registry {
    pub(super) forwarders: RwLock<HashMap<Uuid, ForwarderRuntime>>,
    url_check: std::sync::RwLock<crate::validation::UrlValidator>,
}

impl Registry {
    pub(super) fn new() -> Self {
        Self {
            forwarders: RwLock::new(HashMap::new()),
            url_check: std::sync::RwLock::new(crate::validation::production_url_validator()),
        }
    }

    pub(super) fn set_url_check(&self, v: crate::validation::UrlValidator) {
        *self.url_check.write().unwrap_or_else(|e| e.into_inner()) = v;
    }

    pub(super) fn url_check(&self) -> crate::validation::UrlValidator {
        self.url_check
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Syslog (UDP / TCP)
// ---------------------------------------------------------------------------

/// Build an RFC 5424 syslog message from a forwarder config and audit entry.
/// Shared between UDP and TCP transports to avoid duplicating the message
/// construction logic.
fn build_syslog_message(facility: u8, entry: &AuditEntry, newline: bool) -> String {
    let severity = 6u8; // informational
    let priority = facility * 8 + severity;

    let structured_data = format!(
        "[audit@0 user_id=\"{}\" action=\"{}\" resource=\"{}\" ip=\"{}\"]",
        entry.user_id.as_deref().unwrap_or("-"),
        entry.action,
        entry.resource.as_deref().unwrap_or("-"),
        entry.ip_address.as_deref().unwrap_or("-"),
    );
    let resource = entry.resource.as_deref().unwrap_or("-");

    let mut message = format!(
        "<{priority}>1 {ts} think-watch audit - {action} {sd} {action} on {resource}",
        ts = entry.created_at,
        action = entry.action,
        sd = structured_data,
    );
    if newline {
        message.push('\n');
    }
    message
}

fn parse_syslog_facility(config: &serde_json::Value) -> u8 {
    config
        .get("facility")
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(16) // default: local0
}

pub(super) fn send_udp_syslog(
    runtime: &ForwarderRuntime,
    entry: &AuditEntry,
) -> Result<(), String> {
    let addr = runtime
        .config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in udp_syslog config")?;
    let facility = parse_syslog_facility(&runtime.config.config);
    let socket = runtime
        .udp_socket
        .as_ref()
        .ok_or("UDP socket not initialized")?;
    let message = build_syslog_message(facility, entry, false);

    socket
        .send_to(message.as_bytes(), addr)
        .map(|_| ())
        .map_err(|e| format!("Syslog UDP send failed: {e}"))
}

pub(super) async fn send_tcp_syslog(
    runtime: &ForwarderRuntime,
    entry: &AuditEntry,
) -> Result<(), String> {
    let addr = runtime
        .config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in tcp_syslog config")?;
    let facility = parse_syslog_facility(&runtime.config.config);
    let message = build_syslog_message(facility, entry, true);

    let mut guard = runtime.tcp_stream.lock().await;

    // Try writing to existing stream first
    if let Some(stream) = guard.as_mut() {
        match stream.write_all(message.as_bytes()).await {
            Ok(()) => return Ok(()),
            Err(_) => {
                // Connection is stale, drop it and reconnect below
                *guard = None;
            }
        }
    }

    // Connect (or reconnect after a failed write)
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("TCP syslog connect failed: {e}"))?;
    stream
        .write_all(message.as_bytes())
        .await
        .map_err(|e| format!("TCP syslog write failed: {e}"))?;
    *guard = Some(stream);
    Ok(())
}

// ---------------------------------------------------------------------------
// Kafka (via REST proxy)
// ---------------------------------------------------------------------------

pub(super) async fn send_kafka(
    client: &reqwest::Client,
    check: &crate::validation::UrlValidator,
    config: &LogForwarder,
    entry: &AuditEntry,
) -> Result<(), String> {
    // Kafka via REST proxy (Confluent-compatible)
    let broker_url = config
        .config
        .get("broker_url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'broker_url' in kafka config")?;
    let topic = config
        .config
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'topic' in kafka config")?;

    // DNS rebind defense — same reasoning as `send_webhook`.
    check(broker_url).map_err(|e| format!("URL validation: {e}"))?;

    let payload = serde_json::json!({
        "records": [{
            "value": entry
        }]
    });

    let url = format!("{}/topics/{}", broker_url.trim_end_matches('/'), topic);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/vnd.kafka.json.v2+json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Kafka REST proxy request failed: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Kafka REST proxy returned error: {body}"))
    }
}

// ---------------------------------------------------------------------------
// Webhook (signed HTTP POST)
// ---------------------------------------------------------------------------

pub(super) async fn send_webhook(
    client: &reqwest::Client,
    check: &crate::validation::UrlValidator,
    config: &LogForwarder,
    entry: &AuditEntry,
) -> Result<(), String> {
    let url = config
        .config
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'url' in webhook config")?;

    // DNS rebind defense: re-resolve and re-validate per dispatch.
    // The URL passed validate_url at save time but the hostname is
    // re-resolved on every send — an attacker who controls DNS can
    // flip a benign public A record to 127.0.0.1 / 169.254.169.254
    // between save and any of the up-to-24 retry attempts. Mirrors
    // the test-endpoint pattern. The production check is a no-op DNS
    // hit + CIDR check (sub-ms in steady state) so paying it per
    // delivery is cheap compared to the HTTP round-trip itself.
    check(url).map_err(|e| format!("URL validation: {e}"))?;

    // Serialize the body once so the HMAC signs exactly what goes over
    // the wire — avoids any field-ordering or whitespace divergence
    // between the signature input and the posted body.
    // `Bytes` is cheaply cloneable (ref-counted) so the retry loop
    // doesn't deep-copy the payload on each attempt.
    let body: bytes::Bytes = serde_json::to_vec(entry)
        .map_err(|e| format!("JSON serialise: {e}"))?
        .into();

    // Optional HMAC-SHA256 signature. When `signing_secret` is set on
    // the forwarder row, every delivery carries `x-signature:
    // sha256=<hex>` over `<timestamp>.<body>` and the timestamp as
    // `x-signature-timestamp`. The receiver recomputes it with the same
    // secret and rejects deliveries where `|now - timestamp|` exceeds a
    // window (5 minutes is the recommended one). A mismatch means
    // tampering or a different sender; without the timestamp in the
    // signed input, a captured payload was replayable forever.
    let signing_secret = config
        .config
        .get("signing_secret")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let signature = signing_secret.map(|secret| {
        let mut signed = timestamp.clone().into_bytes();
        signed.push(b'.');
        signed.extend_from_slice(&body);
        hmac_sha256_hex(secret.as_bytes(), &signed)
    });

    // Single attempt — failure parks in `webhook_outbox` for the
    // drain loop to retry with proper exponential backoff (up to 1h,
    // 24-attempt cap). The original 3x inline retry (200/400/800ms)
    // blocked the single audit_worker mpsc consumer for up to 1.4s
    // per entry on a dead receiver, saturating the bounded audit
    // channel and dropping legitimate audit emissions upstream. The
    // outbox already does retry, so the inline loop was both
    // backpressure-fragile and redundant.
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body);

    // Custom headers (new format: JSON object stored as string)
    if let Some(headers_val) = config.config.get("custom_headers") {
        let headers_str = headers_val.as_str().unwrap_or("");
        if let Ok(headers) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_str)
        {
            for (k, v) in headers {
                if let Some(v_str) = v.as_str() {
                    req = req.header(k.as_str(), v_str);
                }
            }
        }
    }

    if let Some(ref sig) = signature {
        req = req
            .header("x-signature", format!("sha256={sig}"))
            .header("x-signature-timestamp", &timestamp);
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => {
            let status = resp.status();
            let rtext = resp.text().await.unwrap_or_default();
            Err(format!("Webhook returned {status}: {rtext}"))
        }
        Err(e) => Err(format!("Webhook request failed: {e}")),
    }
}

/// Hex-encoded HMAC-SHA256. Kept local to this module so the forwarder
/// doesn't pull in a hmac-crate dependency on the shared `common` crate.
/// Implemented via the existing `hmac` workspace dep, re-exported from
/// the auth crate is undesirable (common should not depend on auth).
pub(super) fn hmac_sha256_hex(secret: &[u8], msg: &[u8]) -> String {
    use hmac::{Hmac, Mac, digest::KeyInit};
    type HmacSha256 = Hmac<sha2::Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    hex::encode(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_hex_matches_rfc4231_vector() {
        // RFC 4231 Test Case 1: key = 20 bytes of 0x0b, data = "Hi There".
        // Expected HMAC-SHA256: b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
        let key = [0x0b; 20];
        let got = hmac_sha256_hex(&key, b"Hi There");
        assert_eq!(
            got,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_hex_is_deterministic_and_secret_dependent() {
        let a = hmac_sha256_hex(b"secret-a", b"payload");
        let b = hmac_sha256_hex(b"secret-a", b"payload");
        let c = hmac_sha256_hex(b"secret-b", b"payload");
        assert_eq!(a, b, "same secret + payload must be stable");
        assert_ne!(a, c, "different secret must change the digest");
    }
}
