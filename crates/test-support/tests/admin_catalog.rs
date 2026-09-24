//! The admin catalog endpoints end to end: models, routes, providers and
//! the platform price baseline.
//!
//! Most of these were only reached through the UI before; this file pins
//! what each one reads and writes, so moving their SQL around (into
//! `services::*_repository`) is checked rather than assumed.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// An admin session and a live provider backed by a mock that answers
/// the route-creation probe.
async fn setup(app: &TestApp) -> (TestClient, MockProvider, Uuid) {
    let (con, _) = admin_session_with_user(app).await;
    let upstream = MockProvider::openai_chat_ok("catalog-upstream").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("catalog-prov"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    (con, upstream, provider.id)
}

async fn create_model(con: &TestClient, model_id: &str) -> String {
    let created: Value = con
        .post(
            "/api/admin/models",
            json!({"model_id": model_id, "display_name": model_id}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    created["id"].as_str().expect("model id").to_string()
}

async fn get(con: &TestClient, path: &str) -> Value {
    let resp = con.get(path).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_route_is_created_edited_listed_and_removed() {
    let app = TestApp::spawn().await;
    let (con, _upstream, provider_id) = setup(&app).await;
    let model = unique_name("catalog-model");
    create_model(&con, &model).await;

    let resp = con
        .post(
            &format!("/api/admin/models/{model}/routes"),
            json!({"provider_id": provider_id, "weight": 10, "label": "eu", "notes": ""}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let route: Value = resp.json().unwrap();
    let route_id = route["id"].as_str().unwrap().to_string();
    assert_eq!(route["upstream_model"], model.as_str(), "{route}");
    assert_eq!(route["weight"], 10);
    assert_eq!(route["label"], "eu");
    assert!(
        route.get("notes").is_none(),
        "an empty note is no note: {route}"
    );
    assert!(route["provider_name"].is_string(), "{route}");

    // Same (model, provider, upstream) twice is refused; an unknown
    // model or provider too.
    con.post(
        &format!("/api/admin/models/{model}/routes"),
        json!({"provider_id": provider_id}),
    )
    .await
    .unwrap()
    .assert_status(400);
    con.post(
        &format!("/api/admin/models/{}/routes", unique_name("nope")),
        json!({"provider_id": provider_id}),
    )
    .await
    .unwrap()
    .assert_status(404);
    con.post(
        &format!("/api/admin/models/{model}/routes"),
        json!({"provider_id": Uuid::new_v4(), "upstream_model": "other"}),
    )
    .await
    .unwrap()
    .assert_status(400);

    let routes = get(&con, &format!("/api/admin/models/{model}/routes")).await;
    assert_eq!(routes.as_array().unwrap().len(), 1, "{routes}");

    // PATCH: a JSON null clears, an absent field is left alone.
    let resp = con
        .patch(
            &format!("/api/admin/model-routes/{route_id}"),
            json!({"label": null, "weight": 20, "rpm_cap": 5}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let patched: Value = resp.json().unwrap();
    assert!(patched.get("label").is_none(), "{patched}");
    assert_eq!(patched["weight"], 20);
    assert_eq!(patched["rpm_cap"], 5);
    assert_eq!(patched["enabled"], true);
    con.patch(
        &format!("/api/admin/model-routes/{route_id}"),
        json!({"rpm_cap": 0}),
    )
    .await
    .unwrap()
    .assert_status(400);
    con.patch(
        &format!("/api/admin/model-routes/{}", Uuid::new_v4()),
        json!({"weight": 1}),
    )
    .await
    .unwrap()
    .assert_status(404);

    // The flat listing, unfiltered and filtered both ways.
    let all = get(&con, "/api/admin/model-routes?page_size=200").await;
    assert!(all["total"].as_i64().unwrap() >= 1, "{all}");
    let by_search = get(&con, &format!("/api/admin/model-routes?q={model}")).await;
    assert_eq!(by_search["total"], 1, "{by_search}");
    assert_eq!(by_search["items"][0]["id"], route_id.as_str());
    let by_provider = get(
        &con,
        &format!("/api/admin/model-routes?provider_id={provider_id}"),
    )
    .await;
    assert_eq!(by_provider["total"], 1, "{by_provider}");

    // Batch weights and the batch enable toggle.
    let r: Value = con
        .patch(
            "/api/admin/model-routes/batch-weights",
            json!({"updates": [{"id": route_id, "weight": 7}]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["updated"], 1, "{r}");
    let r: Value = con
        .post(
            "/api/admin/model-routes/batch-update",
            json!({"ids": [route_id], "enabled": false}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["updated"], 1, "{r}");
    let routes = get(&con, &format!("/api/admin/models/{model}/routes")).await;
    assert_eq!(routes[0]["weight"], 7, "{routes}");
    assert_eq!(routes[0]["enabled"], false, "{routes}");

    // A model whose routes are all off lists as disabled.
    let disabled = get(
        &con,
        &format!("/api/admin/models?status=disabled&q={model}"),
    )
    .await;
    assert_eq!(disabled["total"], 1, "{disabled}");
    assert_eq!(disabled["items"][0]["route_count"], 1);
    assert_eq!(disabled["items"][0]["enabled_route_count"], 0);

    let r: Value = con
        .post(
            "/api/admin/model-routes/batch-delete",
            json!({"ids": [route_id]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["deleted"], 1, "{r}");
    let unrouted = get(
        &con,
        &format!("/api/admin/models?status=unrouted&q={model}"),
    )
    .await;
    assert_eq!(unrouted["total"], 1, "{unrouted}");
    con.delete(&format!("/api/admin/model-routes/{route_id}"))
        .await
        .unwrap()
        .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn models_are_listed_toggled_and_deleted_in_bulk() {
    let app = TestApp::spawn().await;
    let (con, _upstream, provider_id) = setup(&app).await;
    let routed = unique_name("bulk-routed");
    let a = unique_name("bulk-a");
    let b = unique_name("bulk-b");
    let c = unique_name("bulk-c");
    create_model(&con, &routed).await;
    let a_id = create_model(&con, &a).await;
    create_model(&con, &b).await;
    let c_id = create_model(&con, &c).await;
    con.post(
        &format!("/api/admin/models/{routed}/routes"),
        json!({"provider_id": provider_id}),
    )
    .await
    .unwrap()
    .assert_ok();

    let active = get(&con, &format!("/api/admin/models?status=active&q={routed}")).await;
    assert_eq!(active["total"], 1, "{active}");
    assert_eq!(active["items"][0]["providers"].as_array().unwrap().len(), 1);

    let ids = get(&con, "/api/admin/models/ids").await;
    let listed: Vec<&str> = ids
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["model_id"].as_str())
        .collect();
    for m in [&routed, &a, &b, &c] {
        assert!(listed.contains(&m.as_str()), "{m} missing from {ids}");
    }

    // Only rows that actually change count.
    for expected in [1, 0] {
        let r: Value = con
            .post(
                "/api/admin/models/bulk-set-enabled",
                json!({"ids": [a_id], "enabled": false}),
            )
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(r["updated"], expected, "{r}");
    }
    let off = get(&con, &format!("/api/admin/models?status=disabled&q={a}")).await;
    assert_eq!(off["items"][0]["enabled"], false, "{off}");

    let r: Value = con
        .post("/api/admin/models/bulk-delete", json!({"ids": [a_id]}))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["deleted"], 1, "{r}");

    let r: Value = con
        .delete(&format!("/api/admin/models/{c_id}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["status"], "deleted", "{r}");

    // `b` has no route; `routed` does and survives.
    let r: Value = con
        .delete("/api/admin/models/unrouted")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(r["deleted"].as_i64().unwrap() >= 1, "{r}");
    let left = get(&con, "/api/admin/models/ids").await;
    let left: Vec<&str> = left
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["model_id"].as_str())
        .collect();
    assert!(left.contains(&routed.as_str()));
    for gone in [&a, &b, &c] {
        assert!(!left.contains(&gone.as_str()), "{gone} still listed");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_provider_edit_forgets_learned_protocols_and_a_delete_drops_its_routes() {
    // The new base URL is the loopback mock again.
    let app = TestApp::spawn_reaching_loopback().await;
    let (con, upstream, provider_id) = setup(&app).await;
    let model = unique_name("prov-model");
    create_model(&con, &model).await;
    con.post(
        &format!("/api/admin/models/{model}/routes"),
        json!({"provider_id": provider_id}),
    )
    .await
    .unwrap()
    .assert_ok();
    let protocol = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT upstream_protocol FROM model_routes WHERE model_id = $1",
        )
        .bind(&model)
        .fetch_one(&app.db)
        .await
        .unwrap()
    };
    assert!(protocol().await.is_some(), "the probe recorded a protocol");

    let one = get(&con, &format!("/api/admin/providers/{provider_id}")).await;
    assert_eq!(one["id"], provider_id.to_string());
    let all = get(&con, "/api/admin/providers").await;
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == provider_id.to_string()),
        "{all}"
    );

    // A display-name change keeps what was learned.
    let resp = con
        .patch(
            &format!("/api/admin/providers/{provider_id}"),
            json!({"display_name": "Renamed"}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let renamed: Value = resp.json().unwrap();
    assert_eq!(renamed["display_name"], "Renamed");
    assert!(protocol().await.is_some());

    // A new base URL may be a different upstream.
    con.patch(
        &format!("/api/admin/providers/{provider_id}"),
        json!({"base_url": upstream.uri()}),
    )
    .await
    .unwrap()
    .assert_ok();
    assert_eq!(protocol().await, None);

    con.delete(&format!("/api/admin/providers/{provider_id}"))
        .await
        .unwrap()
        .assert_ok();
    let routes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_routes WHERE provider_id = $1")
            .bind(provider_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(routes, 0);
    con.get(&format!("/api/admin/providers/{provider_id}"))
        .await
        .unwrap()
        .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_platform_price_baseline_is_read_and_patched() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session_with_user(&app).await;

    let before = get(&con, "/api/admin/platform-pricing").await;
    assert!(before["currency"].is_string(), "{before}");

    let resp = con
        .patch(
            "/api/admin/platform-pricing",
            json!({"input_price_per_token": 0.000002}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let after: Value = resp.json().unwrap();
    assert_eq!(
        after["input_price_per_token"]
            .as_str()
            .map(|s| s.parse::<f64>().unwrap()),
        Some(0.000002),
        "{after}"
    );
    assert_eq!(
        after["output_price_per_token"],
        before["output_price_per_token"]
    );
    assert_eq!(after["currency"], before["currency"]);

    con.patch(
        "/api/admin/platform-pricing",
        json!({"output_price_per_token": -1}),
    )
    .await
    .unwrap()
    .assert_status(400);
}
