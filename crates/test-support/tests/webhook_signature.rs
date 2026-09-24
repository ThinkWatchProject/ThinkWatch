//! Webhook payload signing — `x-signature: sha256=<hex>`.
//!
//! `crates/common/src/audit/forwarders.rs::send_webhook` HMAC-SHA256s
//! `<timestamp>.<body>` with the forwarder's `signing_secret`, sends the
//! result as `x-signature` and the timestamp as `x-signature-timestamp`.
//! Receivers recompute it over the header's timestamp and the raw body,
//! and reject a stale timestamp — without it in the signed input, a
//! captured delivery could be replayed forever.
//!
//! The contract pinned here:
//!
//!   - With `signing_secret` set, every delivery carries
//!     `x-signature: sha256=<hex>` and `x-signature-timestamp`, and the
//!     hex matches HMAC-SHA256 over that timestamp, a `.`, and the
//!     *exact* body bytes the receiver got.
//!   - With `signing_secret` empty / unset, no `x-signature` header
//!     is emitted (back-compat for receivers wired up before signing
//!     was introduced).
//!   - Outbox redelivery (after an inline 3x retry exhausts) re-signs
//!     the same payload, so retries verify against the same secret.
//!   - The `custom_headers` config and the signature coexist — adding
//!     a signing secret must not silently drop user-defined headers.

#![allow(deprecated)] // Tests synthesize audit entries to exercise the pipeline — bare AuditEntry::new is intentional here.

use hmac::{Hmac, Mac, digest::KeyInit};
use sha2::Sha256;
use think_watch_common::audit::AuditEntry;
use think_watch_test_support::prelude::*;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

type HmacSha256 = Hmac<Sha256>;

/// The signature a receiver expects for this delivery: over the
/// timestamp it was sent with, a `.`, and the body. Also checks the
/// timestamp is present and recent, as a receiver would.
fn expected_sig(secret: &[u8], req: &wiremock::Request) -> String {
    let ts = req
        .headers
        .get("x-signature-timestamp")
        .expect("x-signature-timestamp must accompany x-signature")
        .to_str()
        .unwrap();
    let sent: i64 = ts.parse().expect("the timestamp is Unix seconds");
    assert!(
        (chrono::Utc::now().timestamp() - sent).abs() < 300,
        "timestamp {sent} is not recent"
    );
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(&req.body);
    hex::encode(mac.finalize().into_bytes())
}

/// Insert a webhook forwarder directly (the admin POST path's SSRF
/// guard rejects loopback wiremock URLs — same pattern as
/// `webhook_outbox.rs` / `background_tasks.rs`).
async fn install_forwarder(app: &TestApp, config: serde_json::Value) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, log_types, enabled)
           VALUES ($1, 'webhook', $2, ARRAY['audit']::text[], true) RETURNING id"#,
    )
    .bind(unique_name("sig"))
    .bind(config)
    .fetch_one(&app.db)
    .await
    .unwrap();
    app.state.audit.reload_forwarders().await;
    id
}

async fn wait_for_request(receiver: &MockServer) -> Vec<wiremock::Request> {
    for _ in 0..50 {
        let r = receiver.received_requests().await.unwrap_or_default();
        if !r.is_empty() {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("receiver never saw a webhook delivery");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn signature_header_round_trips_hmac_sha256_over_body() {
    let app = TestApp::spawn_reaching_loopback().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let secret = "shh-its-a-secret-42";
    install_forwarder(
        &app,
        json!({
            "url": receiver.uri(),
            "signing_secret": secret,
        }),
    )
    .await;

    app.state
        .audit
        .log(AuditEntry::new("test.signature_round_trip").resource("webhook_signature_test"));

    let received = wait_for_request(&receiver).await;
    let req = &received[0];
    let header = req
        .headers
        .get("x-signature")
        .expect("x-signature header must be present when signing_secret is set")
        .to_str()
        .expect("x-signature value is ASCII");
    let stripped = header
        .strip_prefix("sha256=")
        .unwrap_or_else(|| panic!("x-signature must be prefixed with 'sha256=', got {header}"));

    let want = expected_sig(secret.as_bytes(), req);
    assert_eq!(
        stripped, want,
        "HMAC-SHA256 mismatch: header={stripped} expected={want}"
    );

    // The body the receiver got is JSON and decodes to an AuditEntry.
    // Pin that so a refactor that, say, double-encodes the body (and
    // signs the wrong bytes) is caught.
    let parsed: serde_json::Value = serde_json::from_slice(&req.body).expect("body is JSON");
    assert_eq!(parsed["action"], "test.signature_round_trip");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn no_signature_header_when_signing_secret_unset() {
    let app = TestApp::spawn_reaching_loopback().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    install_forwarder(&app, json!({"url": receiver.uri()})).await;

    app.state
        .audit
        .log(AuditEntry::new("test.no_signature").resource("webhook_signature_test"));

    let received = wait_for_request(&receiver).await;
    assert!(
        received[0].headers.get("x-signature").is_none(),
        "no signing_secret = no x-signature header"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn empty_signing_secret_treated_as_unset() {
    // Belt-and-suspenders: an operator who clears the secret to ""
    // should not accidentally start sending an HMAC computed over an
    // empty key (which would be a constant per-body and worse than
    // no signature at all).
    let app = TestApp::spawn_reaching_loopback().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    install_forwarder(&app, json!({"url": receiver.uri(), "signing_secret": ""})).await;

    app.state
        .audit
        .log(AuditEntry::new("test.empty_secret").resource("webhook_signature_test"));

    let received = wait_for_request(&receiver).await;
    assert!(
        received[0].headers.get("x-signature").is_none(),
        "empty signing_secret must be treated as unset"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn signature_coexists_with_custom_headers() {
    // Adding a signing secret must not silently drop user-defined
    // `custom_headers` (e.g. `Authorization: Bearer …` for receivers
    // that need both auth + signature verification).
    let app = TestApp::spawn_reaching_loopback().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let custom_headers = serde_json::to_string(&json!({
        "X-Tenant": "acme",
        "Authorization": "Bearer downstream-token"
    }))
    .unwrap();

    install_forwarder(
        &app,
        json!({
            "url": receiver.uri(),
            "signing_secret": "k",
            "custom_headers": custom_headers,
        }),
    )
    .await;

    app.state
        .audit
        .log(AuditEntry::new("test.custom_plus_sig").resource("webhook_signature_test"));

    let received = wait_for_request(&receiver).await;
    let req = &received[0];
    assert_eq!(
        req.headers.get("x-tenant").and_then(|v| v.to_str().ok()),
        Some("acme")
    );
    assert_eq!(
        req.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer downstream-token")
    );
    assert!(
        req.headers.get("x-signature").is_some(),
        "signature header still emitted alongside custom headers"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn outbox_redelivery_resigns_payload() {
    // Simulate the failure path: receiver 500s on the first call, then
    // 200s. The drain loop pulls the row out of `webhook_outbox` and
    // re-runs `send_webhook`, which must re-sign the body. Both
    // attempts should carry `x-signature` headers that verify against
    // the body bytes the receiver actually saw on that attempt.
    let app = TestApp::spawn_reaching_loopback().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&receiver)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let secret = "outbox-replay-secret";
    let forwarder_id = install_forwarder(
        &app,
        json!({"url": receiver.uri(), "signing_secret": secret}),
    )
    .await;

    app.state
        .audit
        .log(AuditEntry::new("test.outbox_resign").resource("webhook_signature_test"));

    // Wait for the failed delivery to land in the outbox.
    for _ in 0..50 {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
                .bind(forwarder_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    sqlx::query(
        "UPDATE webhook_outbox SET next_attempt_at = now() - interval '1 second' \
         WHERE forwarder_id = $1",
    )
    .bind(forwarder_id)
    .execute(&app.db)
    .await
    .unwrap();
    app.state.audit.drain_webhook_outbox_once().await.unwrap();

    let received = receiver.received_requests().await.unwrap_or_default();
    assert!(
        received.len() >= 2,
        "expected initial 500 + redelivery, got {}",
        received.len()
    );
    for (i, req) in received.iter().enumerate() {
        let sig = req
            .headers
            .get("x-signature")
            .unwrap_or_else(|| panic!("attempt #{i}: missing x-signature"))
            .to_str()
            .unwrap()
            .strip_prefix("sha256=")
            .unwrap();
        let want = expected_sig(secret.as_bytes(), req);
        assert_eq!(
            sig, want,
            "attempt #{i}: signature must verify against THIS attempt's body bytes"
        );
    }
}
