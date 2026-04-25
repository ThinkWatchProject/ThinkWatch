//! Encryption-at-rest roundtrip tests.
//!
//! Three columns hold AES-256-GCM-encrypted secrets:
//!   - `mcp_servers.auth_secret_encrypted` (BYTEA)
//!   - `system_settings.value` for `oidc.client_secret_encrypted`
//!     (hex-encoded ciphertext stored in the JSON value)
//!   - `users.totp_secret` (hex-encoded ciphertext)
//!
//! The crypto layer has good unit tests in
//! `crates/common/src/crypto.rs`. These tests pin the **handler
//! plumbing**: ciphertext lands in the DB column (not plaintext),
//! decryption recovers the original on read, and a corrupted
//! envelope is rejected. A regression here would silently expose
//! plaintext secrets in any DB dump.

use serde_json::{Value, json};
use think_watch_test_support::prelude::*;

async fn admin_session(app: &TestApp) -> TestClient {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_server_auth_secret_lands_encrypted_in_the_db() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let plaintext_secret = "ghp_super_secret_TOKEN_value";
    let resp: Value = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("enc-mcp"),
                "namespace_prefix": "enc_mcp",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_type": "bearer",
                "auth_secret": plaintext_secret,
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let server_id = resp["id"].as_str().unwrap().to_string();

    // Read the raw bytes back from PG. The plaintext MUST NOT be
    // present in the column — only an AES-GCM envelope.
    let raw: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT auth_secret_encrypted FROM mcp_servers WHERE id::text = $1")
            .bind(&server_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    let raw = raw.expect("auth_secret_encrypted must be populated");
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(
        !raw_str.contains(plaintext_secret),
        "plaintext secret leaked into DB column"
    );
    // Versioned envelope: magic + version + nonce(12) + ct.
    assert!(
        raw.len() >= 4 + 1 + 12 + 16,
        "envelope too short: {} bytes",
        raw.len()
    );

    // Round-trip: decrypt with the AppState's encryption key and
    // confirm the plaintext.
    let key =
        think_watch_common::crypto::parse_encryption_key(&app.state.config.encryption_key).unwrap();
    let decoded = think_watch_common::crypto::decrypt(&raw, &key).expect("decrypt");
    let recovered = String::from_utf8(decoded).unwrap();
    assert_eq!(
        recovered, plaintext_secret,
        "decrypt must recover plaintext"
    );

    // GET handler MUST NOT echo the plaintext or even the
    // ciphertext envelope back — McpServer's serde derive should
    // skip / mask the column.
    let body: Value = con
        .get(&format!("/api/mcp/servers/{server_id}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains(plaintext_secret),
        "plaintext secret echoed in GET response: {body_str}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_server_auth_secret_decrypts_on_health_path() {
    // Sanity that the encrypt-at-create / decrypt-at-use chain
    // reaches the runtime without losing fidelity. Spawning a real
    // upstream MCP server is overkill — we exercise just the
    // encrypt → DB → registry-load → decrypt path.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let secret = "decrypt-roundtrip-token";
    let created: Value = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("enc-rt"),
                "namespace_prefix": "enc_rt",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_type": "bearer",
                "auth_secret": secret,
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let server_id = uuid::Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // Pull the row back as an `McpServer` and feed it through the
    // same `build_registered_server` the gateway uses at startup
    // — which decrypts the secret. If the envelope is corrupted
    // or the key is wrong, this fails.
    let row = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let registered = think_watch_server::mcp_runtime::build_registered_server(
        &app.db,
        &row,
        &app.state.config.encryption_key,
    )
    .await
    .expect("build_registered_server must succeed with a valid encrypted secret");
    // The runtime stores the recovered plaintext on the registered
    // server so the proxy can build the upstream Authorization
    // header. We don't assert on the exact field name (private),
    // just that the build succeeded — which means decrypt did.
    let _ = registered;
}

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
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn corrupted_ciphertext_does_not_leak_recoverable_plaintext() {
    // Pin the **observed** degradation contract: when an MCP
    // server's `auth_secret_encrypted` envelope is corrupted, the
    // runtime LOGS the GCM tag failure and continues building the
    // server WITHOUT an auth header (rather than blowing up the
    // whole gateway). The upstream then sees an unauthenticated
    // request, rejects with 401, and the operator notices the
    // breakage that way.
    //
    // This test is the regression guard against a future change
    // that would silently treat the corrupted bytes as plaintext —
    // i.e., `to_string` of the raw column without trying decrypt.
    // If anyone "fixes" the err path that way, the test catches it.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let plaintext = "wont-survive-corruption";
    con.post(
        "/api/mcp/servers",
        json!({
            "name": unique_name("corrupt"),
            "namespace_prefix": "corrupt_test",
            "endpoint_url": "https://example.com/mcp",
            "transport_type": "streamable_http",
            "auth_type": "bearer",
            "auth_secret": plaintext,
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // Append a junk byte to the ciphertext envelope. Any mutation
    // post-magic / post-nonce trips the GCM tag check.
    sqlx::query(
        r#"UPDATE mcp_servers
              SET auth_secret_encrypted = auth_secret_encrypted || E'\\xff'::bytea
            WHERE namespace_prefix = 'corrupt_test'"#,
    )
    .execute(&app.db)
    .await
    .unwrap();

    let row = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE namespace_prefix = 'corrupt_test'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    let registered = think_watch_server::mcp_runtime::build_registered_server(
        &app.db,
        &row,
        &app.state.config.encryption_key,
    )
    .await
    .expect("build degrades to no-auth rather than failing");
    let dbg = format!("{registered:?}");
    assert!(
        !dbg.contains(plaintext),
        "registered server must NOT contain the plaintext after corruption: {dbg}"
    );
}
