//! The one-time conversion of the guard settings an earlier version kept,
//! end to end against a real database (`guard_policy::legacy`; every
//! mapping is unit-tested there).
//!
//! A database is put back the way an upgraded deployment finds it — the
//! old keys as the old seeds wrote them, the old `models.output_guardrails`
//! column with a cap in it — and migrated again. The new keys hold the
//! converted policies, the old keys and column are gone, a second run
//! changes nothing, and the gateway behaves as the old settings did.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// What the previous version's `db/seeds.sql` wrote, and a tool-call
/// inspection an operator had changed.
async fn put_back_the_old_settings(app: &TestApp) {
    let old = [
        (
            "security.content_filter_patterns",
            json!([
                {"name": "Ignore Previous Instructions", "pattern": "ignore previous instructions", "match_type": "contains", "action": "block"},
                {"name": "Ignore All Previous",          "pattern": "ignore all previous",          "match_type": "contains", "action": "block"},
                {"name": "Disregard Instructions",       "pattern": "disregard your instructions",  "match_type": "contains", "action": "block"},
                {"name": "Jailbreak",                    "pattern": "jailbreak",                    "match_type": "contains", "action": "block"},
                {"name": "DAN",                          "pattern": " dan ",                        "match_type": "contains", "action": "block"},
                {"name": "Developer Mode",               "pattern": "developer mode",               "match_type": "contains", "action": "block"},
                {"name": "Persona Manipulation",         "pattern": "you are now",                  "match_type": "contains", "action": "warn"},
                {"name": "Act As",                       "pattern": "act as",                       "match_type": "contains", "action": "warn"},
                {"name": "System Prompt Extraction",     "pattern": "system prompt",                "match_type": "contains", "action": "warn"},
                {"name": "Reveal Instructions",          "pattern": "reveal your instructions",     "match_type": "contains", "action": "warn"}
            ]),
        ),
        (
            "security.pii_redactor_patterns",
            json!([
                {"name": "email",       "regex": "[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}",    "placeholder_prefix": "EMAIL"},
                {"name": "id_card_cn",  "regex": "\\b\\d{17}[\\dXx]\\b",                                "placeholder_prefix": "ID"},
                {"name": "credit_card", "regex": "\\b\\d{4}[-\\s]?\\d{4}[-\\s]?\\d{4}[-\\s]?\\d{4}\\b", "placeholder_prefix": "CARD"},
                {"name": "phone_cn",    "regex": "1[3-9]\\d{9}",                                         "placeholder_prefix": "PHONE"},
                {"name": "phone_us",    "regex": "\\b\\d{3}[-.]?\\d{3}[-.]?\\d{4}\\b",                   "placeholder_prefix": "PHONE"},
                {"name": "ipv4",        "regex": "\\b\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\b",      "placeholder_prefix": "IP"}
            ]),
        ),
        ("security.hidden_text", json!("block")),
        (
            "security.tool_inspection",
            json!({"mode": "enforce", "disabled": ["chmod-777"], "actions": {"rm-rf-root": "cut"}, "custom": []}),
        ),
    ];
    for (key, value) in old {
        fixtures::set_setting(&app.db, key, value).await.unwrap();
    }
    sqlx::query(
        "ALTER TABLE models ADD COLUMN output_guardrails JSONB NOT NULL DEFAULT '[]'::jsonb",
    )
    .execute(&app.db)
    .await
    .unwrap();
}

async fn setting(app: &TestApp, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(&app.db)
        .await
        .unwrap()
}

async fn has_old_column(app: &TestApp) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_schema = current_schema()
                           AND table_name = 'models' AND column_name = 'output_guardrails')",
    )
    .fetch_one(&app.db)
    .await
    .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_fresh_install_ships_the_factory_policies() {
    let app = TestApp::spawn().await;
    for key in [
        "security.redact",
        "security.inspect_tools",
        "security.content",
    ] {
        assert_eq!(setting(&app, key).await, Some(json!({})), "{key}");
    }
    for key in [
        "security.content_filter_patterns",
        "security.pii_redactor_patterns",
        "security.hidden_text",
        "security.tool_inspection",
    ] {
        assert_eq!(setting(&app, key).await, None, "{key}");
    }
    assert!(!has_old_column(&app).await);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_old_settings_are_converted_once_and_behave_as_before() {
    let app = TestApp::spawn().await;
    put_back_the_old_settings(&app).await;
    let upstream = MockProvider::openai_chat_ok("upgraded-model").await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("up"), "openai", &upstream.uri(), None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "upgraded-model")
        .await
        .unwrap();
    sqlx::query("UPDATE models SET output_guardrails = $1 WHERE model_id = 'upgraded-model'")
        .bind(json!([{"type": "max_length", "max_chars": 1000}]))
        .execute(&app.db)
        .await
        .unwrap();

    think_watch_common::db::run_migrations(&app.db)
        .await
        .unwrap();

    // The old keys and column are gone...
    for key in [
        "security.content_filter_patterns",
        "security.pii_redactor_patterns",
        "security.hidden_text",
        "security.tool_inspection",
    ] {
        assert_eq!(setting(&app, key).await, None, "{key}");
    }
    assert!(!has_old_column(&app).await);
    let cap: Option<i32> = sqlx::query_scalar(
        "SELECT max_output_tokens FROM models WHERE model_id = 'upgraded-model'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(cap, Some(250), "1000 bytes is 250 tokens");

    // ...and the new ones hold what does the same.
    let content = setting(&app, "security.content").await.unwrap();
    assert_eq!(
        content,
        json!({
            "mode": "enforce",
            "enable": ["jailbreak", "dan", "developer-mode", "you-are-now", "act-as",
                       "system-prompt", "reveal-your-instructions"],
            "actions": {"you-are-now": "record", "unicode-tags": "block", "bidi-controls": "block"}
        })
    );
    let redact = setting(&app, "security.redact").await.unwrap();
    assert_eq!(
        redact,
        json!({
            "mode": "enforce",
            "enable": ["email", "cn-mobile-phone"],
            "custom": [
                {"name": "phone_us", "pattern": "\\b\\d{3}[-.]?\\d{3}[-.]?\\d{4}\\b", "label": "PHONE"},
                {"name": "ipv4", "pattern": "\\b\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\b", "label": "IP"}
            ]
        })
    );
    let tools = setting(&app, "security.inspect_tools").await.unwrap();
    assert_eq!(
        tools,
        json!({"mode": "enforce", "disable": ["chmod-777"], "actions": {"rm-rf-root": "cut"}})
    );

    // A second run finds nothing to do.
    think_watch_common::db::run_migrations(&app.db)
        .await
        .unwrap();
    assert_eq!(setting(&app, "security.content").await.unwrap(), content);
    assert_eq!(setting(&app, "security.redact").await.unwrap(), redact);
    assert_eq!(
        setting(&app, "security.inspect_tools").await.unwrap(),
        tools
    );

    // The gateway, running on what was converted, does what it did.
    app.state.dynamic_config.reload().await.unwrap();
    think_watch_server::app::reload_guards(&app.state).await;
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "up", &["ai_gateway"], None, None)
        .await
        .unwrap()
        .plaintext;
    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let refused = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "upgraded-model", "messages": [
                {"role": "user", "content": "try a jailbreak"}
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(refused.status.as_u16(), 403, "{}", refused.text());
    assert!(upstream.received_requests().await.is_empty());

    gw.post(
        "/v1/chat/completions",
        json!({"model": "upgraded-model", "temperature": 0.5, "messages": [
            {"role": "user", "content": "write to alice@example.com"}
        ]}),
    )
    .await
    .unwrap()
    .assert_ok();
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["messages"][0]["content"], "write to <<TW_EMAIL_1>>");
    assert_eq!(sent["max_tokens"], 250, "{sent}");
}
