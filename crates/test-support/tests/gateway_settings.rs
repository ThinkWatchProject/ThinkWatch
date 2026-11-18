use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "gateway-test-model";
const WEB: &str = "gateway.web_tools";
const DISCOVERY: &str = "gateway.model_discovery";

async fn seed_route(app: &TestApp, upstream: &MockServer, provider_type: &str) -> TestClient {
    let provider = fixtures::create_provider(
        &app.db,
        "gateway-settings-provider",
        provider_type,
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, MODEL)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "settings-test",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn concurrent_gateway_saves_detect_conflicts_and_reject_invalid_aliases() {
    let app = TestApp::spawn().await;
    let upstream = MockServer::start().await;
    let gw = seed_route(&app, &upstream, "openai").await;
    let first = admin_session(&app).await;
    let second = admin_session(&app).await;
    let settings = "/api/admin/settings";

    for (name, target, message) in [
        (MODEL, MODEL, "Aliases require exact model IDs"),
        (MODEL, "missing-model", "conflicts with an existing model"),
        ("friendly", "missing-model", "has no active route"),
        (
            "friendly",
            "gateway-test-model-extra",
            "has no active route",
        ),
    ] {
        let result = first
            .patch(settings, json!({"settings":{DISCOVERY:{"aliases":{name:{"target":target}}}},"expected":{DISCOVERY:{}}}))
            .await
            .unwrap();
        result.assert_status(400);
        assert!(result.text().contains(message), "{}", result.text());
    }

    let change_a = json!({"settings":{DISCOVERY:{"aliases":{"fast":{"target":MODEL}}}},"expected":{DISCOVERY:{}}});
    let change_b = json!({"settings":{DISCOVERY:{"aliases":{"code":{"target":MODEL}}}},"expected":{DISCOVERY:{}}});
    let (a, b) = tokio::join!(
        first.patch(settings, change_a),
        second.patch(settings, change_b)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let winning_name = if a.status.is_success() {
        b.assert_status(409);
        "fast"
    } else {
        a.assert_status(409);
        b.assert_ok();
        "code"
    };
    let saved: Json = sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1")
        .bind(DISCOVERY)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(saved, json!({"aliases":{winning_name:{"target":MODEL}}}));

    let models: Json = gw
        .get("/v1/models")
        .await
        .unwrap()
        .assert_ok()
        .json()
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == winning_name)
    );
    sqlx::query("INSERT INTO models (model_id, display_name) VALUES ($1, $1)")
        .bind(winning_name)
        .execute(&app.db)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let models: Json = gw
        .get("/v1/models")
        .await
        .unwrap()
        .assert_ok()
        .json()
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["id"] != winning_name)
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn codex_catalog_preserves_native_fields_and_partitions_client_versions() {
    let app = TestApp::spawn().await;
    let upstream = MockServer::start().await;
    let gw = seed_route(&app, &upstream, "openai").await;
    for version in ["0.165.0", "0.166.0"] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("client_version", version))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
                "slug": MODEL, "display_name": "Upstream model",
                "base_instructions": format!("Native instructions for {version}"),
                "shell_type":"shell_command", "experimental_supported_tools":["apply_patch"],
                "truncation_policy":{"mode":"tokens","limit":8000},
                "input_modalities":["text","image"], "context_window":120000,
                "supported_reasoning_levels":[{"effort":"medium","description":"Upstream explanation"}],
                "default_reasoning_level":"medium"
            }]})))
            .expect(1)
            .mount(&upstream)
            .await;
    }
    for version in ["0.165.0", "0.166.0", "0.165.0"] {
        let response = gw
            .get(&format!("/codex/models?client_version={version}"))
            .await
            .unwrap();
        let catalog: Json = response.assert_ok().json().unwrap();
        let model = &catalog["models"][0];
        assert_eq!(model["slug"], MODEL);
        assert_eq!(
            model["base_instructions"],
            format!("Native instructions for {version}")
        );
        assert_eq!(
            model["experimental_supported_tools"],
            json!(["apply_patch"])
        );
        assert_eq!(
            model["supported_reasoning_levels"][0]["description"],
            "Upstream explanation"
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn hosted_fallback_counts_once_and_old_history_survives_disabling_tools() {
    let app = TestApp::spawn().await;
    let upstream = MockServer::start().await;
    let gw = seed_route(&app, &upstream, "anthropic").await;
    let (key, owner): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, user_id FROM api_keys WHERE name = 'settings-test'")
            .fetch_one(&app.db)
            .await
            .unwrap();
    for (kind, subject) in [("user", owner), ("api_key_lineage", key)] {
        fixtures::create_rate_limit_rule(&app.db, kind, subject, "ai_gateway", "requests", 60, 1)
            .await
            .unwrap();
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(|req: &Request| String::from_utf8_lossy(&req.body).contains("\"name\":\"thinkwatch_open_page\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_tool","type":"message","role":"assistant","model":MODEL,"stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"toolu_read","name":"thinkwatch_open_page","input":{"url":"http://127.0.0.1/"}}],
            "usage":{"input_tokens":10,"output_tokens":5}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(|req: &Request| {
            let raw = String::from_utf8_lossy(&req.body);
            raw.contains("Previously retrieved web evidence") && !raw.contains("srvtoolu_tw_")
                && !raw.contains("\"name\":\"thinkwatch_open_page\"")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_final","type":"message","role":"assistant","model":MODEL,"stop_reason":"end_turn",
            "content":[{"type":"text","text":"continued"}],"usage":{"input_tokens":15,"output_tokens":2}
        })))
        .expect(2)
        .mount(&upstream)
        .await;

    let con = admin_session(&app).await;
    let enabled =
        json!({"execution":{"mode":"core","fallback_to_upstream":true},"enabled_lineages":[key]});
    con.patch(
        "/api/admin/settings",
        json!({"settings":{WEB:enabled},"expected":{WEB:{}}}),
    )
    .await
    .unwrap()
    .assert_ok();
    let diagnostic = con
        .post(
            "/api/admin/settings/web-tools/test",
            json!({"action":"open_page","url":"http://127.0.0.1/"}),
        )
        .await
        .unwrap();
    diagnostic.assert_status(502);
    assert!(
        diagnostic.text().contains("public"),
        "{}",
        diagnostic.text()
    );

    let mut request = json!({
        "model":MODEL,"max_tokens":128,
        "tools":[{"type":"web_fetch_20250910","name":"web_fetch"}],
        "messages":[
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"srvtoolu_tw_old","name":"web_search","input":{"query":"test"}},
                {"type":"web_search_tool_result","tool_use_id":"srvtoolu_tw_old","content":[{"type":"web_search_result","url":"https://example.org/","title":"Example","encrypted_content":""}]}
            ]},
            {"role":"user","content":"Continue."}
        ]
    });
    let first = gw.post("/v1/messages", &request).await.unwrap();
    let answer: Json = first.assert_ok().json().unwrap();
    assert_eq!(answer["content"][0]["text"], "continued");
    gw.post("/v1/messages", &request)
        .await
        .unwrap()
        .assert_status(429);

    con.patch(
        "/api/admin/settings",
        json!({"settings":{WEB:{}},"expected":{WEB:enabled}}),
    )
    .await
    .unwrap()
    .assert_ok();
    sqlx::query("DELETE FROM rate_limit_rules WHERE subject_id IN ($1, $2)")
        .bind(key)
        .bind(owner)
        .execute(&app.db)
        .await
        .unwrap();
    request.as_object_mut().unwrap().remove("tools");
    gw.post("/v1/messages", &request).await.unwrap().assert_ok();
}
