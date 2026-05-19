//! End-to-end test for the S3 offload path landed in 0fe7fb5 / 3d39a30.
//!
//! Strategy: inject an in-memory `BlobStore` (no real RustFS / S3
//! needed) via `SpawnOptions::blob_store`, drive a chat completion with
//! `audit.body_max_bytes` lowered below the JSON prompt size, and
//! assert:
//!
//!   * the audit row carries `body_capture_status = "offloaded"`
//!   * the body cell stores an `s3://bucket/key` URL (NOT the truncated
//!     payload + "..." sentinel that the no-store fallback would write)
//!   * the in-memory store actually received the upload
//!   * the body-viewer endpoint dereferences the URL and returns the
//!     original payload bytes
//!
//! Like the other CH tests it's `#[ignore]` so `cargo nextest run
//! --workspace` skips it; `make test-it` opts in.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use think_watch_common::blob_store::{BlobDecision, BlobError, BlobKeyHint, BlobStore};
use think_watch_test_support::prelude::*;
use tokio::sync::Mutex;

const BUCKET: &str = "audit-bodies-test";
const PROBE_PROMPT: &str = "tell me about clickhouse body offload";

/// In-memory `BlobStore` for the offload path. Captures every upload
/// into a `HashMap<key, body>` so the test can assert what was sent;
/// `fetch` serves them back. Deliberately minimal — no MIME, no
/// metadata; we're testing the proxy's offload decision, not the
/// store mechanics.
#[derive(Debug, Default)]
struct InMemoryBlobStore {
    contents: Mutex<HashMap<String, Vec<u8>>>,
}

#[async_trait]
impl BlobStore for InMemoryBlobStore {
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
        let bytes = content.len();
        // Mirror S3Store's key shape so the proxy + viewer produce
        // identical URLs whether they hit this fake or a real bucket.
        let key = format!(
            "bodies/{}/test/{}-{}.json",
            hint.table, hint.log_id, hint.field
        );
        let url = format!("s3://{}/{}", BUCKET, key);
        self.contents.lock().await.insert(key, content.into_bytes());
        Ok(BlobDecision::Offloaded { url, bytes })
    }
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, BlobError> {
        // Same parsing rule S3Store enforces — refuse cross-bucket.
        let key = url
            .strip_prefix(&format!("s3://{BUCKET}/"))
            .ok_or_else(|| BlobError::BadUrl(format!("wrong bucket: {url}")))?;
        self.contents
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| BlobError::BadUrl(format!("not stored: {url}")))
    }
    async fn smoke_test(&self) -> Result<(), BlobError> {
        // Always healthy — the in-memory store has no network surface
        // to misconfigure, so smoke_test is a no-op success.
        Ok(())
    }
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct OffloadRow {
    request_body: Option<String>,
    // Pulled from the SELECT for column-order parity with the audit
    // schema; the test asserts on request_body + status only, so the
    // response body field is intentionally unused here.
    #[allow(dead_code)]
    response_body: Option<String>,
    body_capture_status: Option<String>,
}

async fn seed_runtime(app: &TestApp) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let uri = mock.uri();
    Box::leak(Box::new(mock));

    let provider =
        fixtures::create_provider(&app.db, &unique_name("offload-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("off-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oversize_body_offloads_to_blob_store_and_dereferences_via_endpoint() {
    // Stand up TestApp with an in-memory blob store wired in. No real
    // RustFS / S3 needed — the BlobStore trait is the seam.
    let store = Arc::new(InMemoryBlobStore::default());
    let store_dyn: Arc<dyn BlobStore> = store.clone();
    let app = TestApp::try_spawn_with(SpawnOptions {
        clickhouse: true,
        blob_store: Some(store_dyn),
        ..Default::default()
    })
    .await
    .expect("spawn with custom blob_store");

    // Force the request body to exceed the inline cap. The serialized
    // [{ "role":"user", "content":"..." }] envelope adds ~30 bytes, so
    // 200 + envelope > 100.
    fixtures::set_setting(&app.db, "audit.body_max_bytes", Value::from(100_i64))
        .await
        .unwrap();
    let (api_key, user_id) = seed_runtime(&app).await;
    let big_prompt = format!("{} {}", PROBE_PROMPT, "x".repeat(400));

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post(
        "/v1/chat/completions",
        json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": big_prompt}],
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let ch = app.state.clickhouse.as_ref().expect("clickhouse wired");
    let row = poll_offload_row(ch, user_id).await;

    assert_eq!(
        row.body_capture_status.as_deref(),
        Some("offloaded"),
        "status should be `offloaded` when body > cap and offload is available"
    );
    let request_url = row.request_body.expect("request body cell populated");
    assert!(
        request_url.starts_with("s3://audit-bodies-test/bodies/gateway_logs/"),
        "request_body cell should hold an s3:// URL, got: {request_url}"
    );
    assert!(
        !request_url.contains("..."),
        "URL must not be a truncated payload — got: {request_url}"
    );

    // The in-memory store must have actually received the upload.
    let stored_keys: Vec<String> = store.contents.lock().await.keys().cloned().collect();
    assert!(
        stored_keys.iter().any(|k| k.ends_with("-request.json")),
        "blob store should have received a `-request.json` upload, got keys: {stored_keys:?}"
    );

    // Body-viewer endpoint dereferences. Need a super_admin session
    // because logs:read_bodies isn't in the default `admin` policy;
    // `create_admin_user` assigns the super_admin role specifically.
    // Cookie-based login (reqwest client persists the access_token
    // cookie across calls — same pattern as console_admin.rs).
    let admin_user = fixtures::create_admin_user(&app.db).await.unwrap();
    let admin = app.console_client();
    admin
        .post(
            "/api/auth/login",
            json!({
                "email": admin_user.user.email,
                "password": admin_user.plaintext_password,
            }),
        )
        .await
        .expect("admin login")
        .assert_ok();
    let row_id = poll_row_id(ch, user_id).await;
    let body_resp = admin
        .get(&format!("/api/admin/gateway/logs/{row_id}/body"))
        .await
        .expect("body endpoint");
    body_resp.assert_ok();
    let body_json: Value = body_resp.json().expect("body json");
    let resolved = body_json
        .get("request_body")
        .and_then(Value::as_str)
        .expect("resolved request_body in body endpoint response");
    assert!(
        resolved.contains(PROBE_PROMPT),
        "dereferenced body should contain the original prompt — got first 200: {:?}",
        &resolved.chars().take(200).collect::<String>()
    );
    assert!(
        !resolved.starts_with("s3://"),
        "viewer endpoint must DEREF s3:// URLs — got raw URL back: {resolved}"
    );
}

async fn poll_offload_row(ch: &clickhouse::Client, user_id: uuid::Uuid) -> OffloadRow {
    for _ in 0..200 {
        let row: Option<OffloadRow> = ch
            .query(
                "SELECT request_body, response_body, body_capture_status \
                   FROM gateway_logs \
                  WHERE user_id = ? AND body_capture_status = 'offloaded' \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some(r) = row {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("offloaded row never landed for user {user_id}");
}

async fn poll_row_id(ch: &clickhouse::Client, user_id: uuid::Uuid) -> String {
    #[derive(Debug, Deserialize, clickhouse::Row)]
    struct IdRow {
        id: String,
    }
    for _ in 0..200 {
        let row: Option<IdRow> = ch
            .query(
                "SELECT id FROM gateway_logs \
                  WHERE user_id = ? AND body_capture_status = 'offloaded' \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some(r) = row {
            return r.id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no row id for user {user_id}");
}
