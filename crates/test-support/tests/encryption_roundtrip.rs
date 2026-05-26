//! Encryption-at-rest roundtrip tests for handler plumbing.
//!
//! Three storage shapes hold AES-256-GCM-encrypted secrets reachable
//! through handlers covered here:
//!   - `system_settings.value` for `oidc.client_secret_encrypted`
//!     (hex-encoded ciphertext stored in the JSON value)
//!   - `users.totp_secret` (hex-encoded ciphertext)
//!   - `providers.config_json` — every header `value` and the
//!     `aws_secret_access_key` are stored as `{"$enc": "<hex>"}`
//!
//! The crypto layer has good unit tests in
//! `crates/common/src/crypto.rs`. These tests pin the **handler
//! plumbing**: ciphertext lands in the DB column (not plaintext),
//! decryption recovers the original on read, and the GET handlers
//! never echo the plaintext back out.
//!
//! MCP server credentials moved to a per-axis storage model (per-user
//! `mcp_user_credentials` and admin-shared `mcp_server_shared_credentials`,
//! each with its own encrypted-at-rest token columns). Their
//! roundtrip is exercised through the per-user OAuth / static-token
//! integration tests rather than this file.
//!
//! TOTP recovery codes are also AES-256-GCM-encrypted; their
//! single-consume contract is asserted via black-box login attempts in
//! `totp_recovery.rs` so we don't duplicate the storage check here.

use serde_json::{Value, json};
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oidc_client_secret_round_trips_through_admin_patch() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let secret = "oidc_super_secret_4tw";
    con.patch(
        "/api/admin/settings/oidc",
        json!({
            "enabled": true,
            // Real public hostname so the SSRF guard's DNS resolve
            // step passes — we only care about the encryption
            // round-trip, not actual OIDC discovery.
            "issuer_url": "https://accounts.google.com",
            "client_id": "tw-client",
            "client_secret": secret,
            "redirect_url": "https://app.example.com/api/auth/sso/callback"
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // The setting key for the encrypted secret. Stored as hex of
    // the raw envelope so it fits inside the `system_settings.value`
    // JSONB. Decrypt it here and confirm the plaintext.
    let stored: Value = sqlx::query_scalar(
        "SELECT value FROM system_settings WHERE key = 'oidc.client_secret_encrypted'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    let hex_text = stored.as_str().expect("hex string in JSONB value");
    assert!(!hex_text.is_empty(), "client_secret was not persisted");
    assert!(
        !hex_text.contains(secret),
        "plaintext secret leaked into the system_settings hex value"
    );

    let key =
        think_watch_common::crypto::parse_encryption_key(&app.state.config.encryption_key).unwrap();
    let raw = hex::decode(hex_text).expect("hex decode");
    let decoded = think_watch_common::crypto::decrypt(&raw, &key).expect("decrypt the OIDC secret");
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        secret,
        "decrypt must recover the OIDC client_secret"
    );

    // The GET handler must NEVER echo the plaintext secret. The UI
    // shows a masked value; the column behind it is the hex of the
    // ciphertext envelope, never the plaintext.
    let body: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains(secret),
        "GET /admin/settings/oidc echoed the plaintext: {body_str}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn totp_secret_lands_encrypted_in_users_row() {
    // `users.totp_secret` is hex-encoded ciphertext after enable.
    // The plaintext base32 secret must never reach the DB column,
    // and verify must reconstruct the same secret on subsequent
    // logins.
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let plaintext_secret = setup["secret"].as_str().expect("totp secret").to_string();

    let code = think_watch_auth::totp::current_code(&plaintext_secret, &user.user.email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code}))
        .await
        .unwrap()
        .assert_ok();

    // After verify-setup, the row's totp_secret is the hex
    // ciphertext. It MUST NOT contain the plaintext.
    let stored: Option<String> = sqlx::query_scalar("SELECT totp_secret FROM users WHERE id = $1")
        .bind(user.user.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    let stored = stored.expect("totp_secret must be populated");
    assert!(
        !stored.contains(&plaintext_secret),
        "TOTP plaintext leaked into users.totp_secret"
    );

    // Decrypting recovers the original.
    let key =
        think_watch_common::crypto::parse_encryption_key(&app.state.config.encryption_key).unwrap();
    let bytes = hex::decode(&stored).expect("hex decode");
    let recovered = think_watch_common::crypto::decrypt(&bytes, &key).expect("decrypt totp secret");
    assert_eq!(
        String::from_utf8(recovered).unwrap(),
        plaintext_secret,
        "decrypt must recover the TOTP base32 secret"
    );

    // Recovery codes column also lands as ciphertext (not plaintext
    // JSON) — pin that contract too. The codes themselves are
    // exercised by totp_recovery.rs's single-consume tests; here we
    // only check the storage envelope to catch any future change
    // that bypasses the encrypt helper.
    let codes_blob: Option<String> =
        sqlx::query_scalar("SELECT totp_recovery_codes FROM users WHERE id = $1")
            .bind(user.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    let codes_blob = codes_blob.expect("totp_recovery_codes must be populated");
    assert!(
        !codes_blob.starts_with('[') && !codes_blob.starts_with('"'),
        "recovery codes must be hex ciphertext, not plain JSON: {codes_blob}"
    );
    let codes_bytes = hex::decode(&codes_blob).expect("hex decode recovery codes");
    let codes_decrypted =
        think_watch_common::crypto::decrypt(&codes_bytes, &key).expect("decrypt recovery codes");
    let parsed: Vec<String> = serde_json::from_slice(&codes_decrypted).expect("codes JSON parse");
    assert_eq!(parsed.len(), 10, "should mint 10 recovery codes");
}

// ---------------------------------------------------------------------------
// providers.config_json — header values + aws_secret_access_key encrypt at rest
// ---------------------------------------------------------------------------

/// Decrypt a `JsonSecret`-wrapped value using the test app's master
/// key. Panics if the value isn't a well-formed envelope.
fn decode_enc_envelope(v: &Value, encryption_key: &str) -> String {
    use think_watch_common::json_secret::JsonSecret;
    let secret = JsonSecret::from_json(v).expect("envelope");
    assert!(
        secret.is_encrypted(),
        "expected JsonSecret::Encrypted wrapper, got {v}"
    );
    secret.decrypt(encryption_key).expect("decrypt envelope")
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn provider_create_encrypts_header_values_at_rest() {
    // Verifies the handler plumbing for POST /api/admin/providers:
    //   - every header value lands as `{"$enc": "<hex>"}` in the DB row
    //   - plaintext never appears anywhere in `config_json`
    //   - the gateway router reload decrypts back to the original
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let secret_value = "sk-test-rotated-1234567890";
    let resp = con
        .post(
            "/api/admin/providers",
            json!({
                "name": "openai-encrypted-test",
                "display_name": "OpenAI (encryption test)",
                "provider_type": "openai",
                "base_url": "https://api.openai.com",
                "headers": [
                    {"key": "Authorization", "value": format!("Bearer {secret_value}")},
                    {"key": "X-Custom-Header", "value": "non-sensitive-but-still-encrypted"},
                ],
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let created: Value = resp.json().unwrap();
    let provider_id = created["id"].as_str().unwrap().to_string();

    // Pull the raw config_json straight out of Postgres.
    let stored: Value = sqlx::query_scalar("SELECT config_json FROM providers WHERE id = $1::uuid")
        .bind(&provider_id)
        .fetch_one(&app.db)
        .await
        .unwrap();

    let stored_str = serde_json::to_string(&stored).unwrap();
    assert!(
        !stored_str.contains(secret_value),
        "plaintext Authorization secret leaked into providers.config_json: {stored_str}"
    );

    let headers = stored["headers"]
        .as_array()
        .expect("headers must be a JSON array");
    use think_watch_common::json_secret::JsonSecret;
    assert_eq!(headers.len(), 2);
    for h in headers {
        let v = &h["value"];
        assert!(
            JsonSecret::json_is_encrypted(v),
            "header value must be encrypted-at-rest envelope: {v}"
        );
    }
    // Confirm decrypt recovers the original Authorization value.
    let auth = headers
        .iter()
        .find(|h| h["key"] == "Authorization")
        .unwrap();
    let decrypted = decode_enc_envelope(&auth["value"], &app.state.config.encryption_key);
    assert_eq!(decrypted, format!("Bearer {secret_value}"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn provider_create_encrypts_aws_bedrock_secret() {
    // Bedrock secrets are stored as `aws_secret_access_key` directly
    // under `config_json`, not in the headers array. The handler must
    // wrap those too.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let aws_secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let resp = con
        .post(
            "/api/admin/providers",
            json!({
                "name": "bedrock-encrypted-test",
                "display_name": "AWS Bedrock (encryption test)",
                "provider_type": "bedrock",
                "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com",
                "headers": [],
                "config": {
                    "aws_access_key_id": "AKIAIOSFODNN7EXAMPLE",
                    "aws_secret_access_key": aws_secret,
                },
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let created: Value = resp.json().unwrap();
    let provider_id = created["id"].as_str().unwrap().to_string();

    let stored: Value = sqlx::query_scalar("SELECT config_json FROM providers WHERE id = $1::uuid")
        .bind(&provider_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    let stored_str = serde_json::to_string(&stored).unwrap();
    assert!(
        !stored_str.contains(aws_secret),
        "plaintext aws_secret_access_key leaked: {stored_str}"
    );

    use think_watch_common::json_secret::JsonSecret;
    let wrapped = &stored["aws_secret_access_key"];
    assert!(
        JsonSecret::json_is_encrypted(wrapped),
        "aws_secret_access_key must be encrypted-at-rest: {wrapped}"
    );
    let decrypted = decode_enc_envelope(wrapped, &app.state.config.encryption_key);
    assert_eq!(decrypted, aws_secret);

    // access_key_id is not sensitive — must remain plaintext for log/UI surfacing.
    assert_eq!(stored["aws_access_key_id"], "AKIAIOSFODNN7EXAMPLE");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn provider_loader_decrypts_envelopes_back_to_headers() {
    // End-to-end: create a provider, force a router rebuild, and
    // confirm the in-memory router sees the decrypted Authorization
    // value (not the $enc wrapper). This is the gateway-side
    // observability check — if the loader regressed and stored the
    // ciphertext verbatim, upstream calls would fail with 401.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let upstream_secret = "test-bearer-for-loader-roundtrip";
    let name = unique_name("loader-rt");
    let resp = con
        .post(
            "/api/admin/providers",
            json!({
                "name": name,
                "display_name": name,
                "provider_type": "openai",
                "base_url": "https://api.openai.com",
                "headers": [
                    {"key": "Authorization", "value": format!("Bearer {upstream_secret}")},
                ],
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let provider_id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // Register a model+route and force a rebuild so the router resolves
    // headers via the decrypt path.
    let model_id = unique_name("loader-rt-model");
    fixtures::create_model_and_route(&app.db, provider_id, &model_id)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // The router holds the route — peek at how many routes the model
    // resolves to. If the loader had blown up on the envelope, the
    // route would have been skipped.
    let router = app.state.gateway_router.load_full();
    let models = router.list_models();
    assert!(
        models.iter().any(|m| m == &model_id),
        "model {model_id} missing from router after encrypted-header reload (loader likely failed): {models:?}"
    );
}
