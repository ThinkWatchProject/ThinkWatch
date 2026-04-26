//! API key rotation contract.
//!
//! `console_admin.rs::api_keys_lifecycle` exercises the rotate
//! endpoint as part of the broader CRUD smoke. This file pins the
//! rotation-specific properties:
//!
//!   - The new key's `name` matches the old key VERBATIM (no
//!     " (rotated)" suffix). An earlier implementation appended the
//!     literal string into the column with no removal logic, so
//!     repeated rotations produced names like
//!     "Foo (rotated) (rotated) (rotated)…". The status badge in
//!     the UI already tells the operator which generation is which;
//!     stamping the same fact into `name` was duplicate signalling
//!     that mutated state.
//!   - Lineage (`rotated_from_id`, `last_rotation_at`) is set on
//!     the new key so the UI / analytics can join across
//!     generations later if it wants to roll up usage.
//!   - The old key keeps its name, gains a `grace_period_ends_at`,
//!     and is flagged with `disabled_reason = 'rotated'` so
//!     callers — and the eventual reaper — know the key is on its
//!     way out.

use chrono::{DateTime, Utc};
use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rotation_preserves_name_and_records_lineage() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let original_name = "My Production Key";
    let create: Value = con
        .post(
            "/api/keys",
            json!({"name": original_name, "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let old_id = create["id"]
        .as_str()
        .expect("create returns id")
        .to_string();

    // Rotate.
    let rotated: Value = con
        .post_empty(&format!("/api/keys/{old_id}/rotate"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let new_id = rotated["id"]
        .as_str()
        .expect("rotate returns new id")
        .to_string();
    assert_ne!(new_id, old_id, "rotation must mint a fresh UUID");

    // 1) New key's `name` is the original verbatim.
    assert_eq!(
        rotated["name"], original_name,
        "rotation must NOT append a suffix to the new key's name; got {rotated}"
    );

    // 2) DB-side lineage on the new key.
    let row: (String, Option<uuid::Uuid>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT name, rotated_from_id, last_rotation_at FROM api_keys WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&new_id).unwrap())
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(row.0, original_name);
    assert_eq!(
        row.1.map(|u| u.to_string()).as_deref(),
        Some(old_id.as_str()),
        "rotated_from_id must point at the old key for lineage joins"
    );
    assert!(
        row.2.is_some(),
        "last_rotation_at must be stamped at rotation time"
    );

    // 3) Old key gets a grace-period schedule + disabled_reason.
    let old: (String, Option<DateTime<Utc>>, Option<String>) = sqlx::query_as(
        "SELECT name, grace_period_ends_at, disabled_reason FROM api_keys WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&old_id).unwrap())
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(old.0, original_name, "old key keeps its original name too");
    assert!(
        old.1.is_some(),
        "old key must get grace_period_ends_at — without it the rotation reaper never retires it"
    );
    assert_eq!(old.2.as_deref(), Some("rotated"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn repeated_rotations_do_not_stack_suffixes() {
    // Regression guard for the earlier "(rotated) (rotated) (rotated)…"
    // bug: a key rotated N times must have the same `name` as the
    // original on every generation.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let name = "Long-lived Key";
    let create: Value = con
        .post(
            "/api/keys",
            json!({"name": name, "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let mut current_id = create["id"].as_str().unwrap().to_string();

    for n in 1..=4 {
        let rotated: Value = con
            .post_empty(&format!("/api/keys/{current_id}/rotate"))
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(
            rotated["name"], name,
            "generation #{n} must carry the original name verbatim, got {rotated}"
        );
        current_id = rotated["id"].as_str().unwrap().to_string();
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn lineage_id_is_stable_across_every_rotation_generation() {
    // Create a key, rotate four times, and assert every row in the
    // chain shares the same `lineage_id`. Pin the root invariant
    // (`id == lineage_id` for the brand-new key) too so a refactor
    // that drifts the create path is caught.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let create: Value = con
        .post(
            "/api/keys",
            json!({"name": "Lineage Probe", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let root_id = create["id"].as_str().unwrap().to_string();

    let root_lineage: uuid::Uuid =
        sqlx::query_scalar("SELECT lineage_id FROM api_keys WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&root_id).unwrap())
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        root_lineage.to_string(),
        root_id,
        "fresh key must have lineage_id == id (root-of-chain invariant)"
    );

    let mut chain: Vec<uuid::Uuid> = vec![uuid::Uuid::parse_str(&root_id).unwrap()];
    let mut current = root_id.clone();
    for _ in 0..4 {
        let rotated: Value = con
            .post_empty(&format!("/api/keys/{current}/rotate"))
            .await
            .unwrap()
            .json()
            .unwrap();
        current = rotated["id"].as_str().unwrap().to_string();
        chain.push(uuid::Uuid::parse_str(&current).unwrap());
    }

    // Every row in the chain shares one lineage_id.
    let lineages: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT lineage_id FROM api_keys WHERE id = ANY($1)")
            .bind(&chain)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(lineages.len(), 5, "all 5 generations must be visible");
    for lid in &lineages {
        assert_eq!(
            lid, &root_lineage,
            "every generation must share the root's lineage_id; got {lid}"
        );
    }

    // Every descendant must NOT have id == lineage_id (only the root does).
    let non_root: Vec<uuid::Uuid> = chain.iter().skip(1).copied().collect();
    let any_self_lineage: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE id = ANY($1) AND id = lineage_id")
            .bind(&non_root)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        any_self_lineage, 0,
        "rotated descendants must NOT match id == lineage_id; that's the root's invariant only"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gateway_logs_lineage_filter_rolls_up_history_across_rotations() {
    // The whole point of lineage_id: a request issued under
    // generation N and queried under generation M (or any
    // generation) by lineage_id pulls the row back. Verifies the
    // CH side's `api_key_lineage_id` column is populated and
    // matches between Rust write + Rust read.
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Stand up a real upstream so the gateway proxy emits a
    // gateway_logs row.
    let upstream = MockProvider::openai_chat_ok("lineage-test-model").await;
    let uri = upstream.uri();
    Box::leak(Box::new(upstream));
    let provider =
        fixtures::create_provider(&app.db, &unique_name("lin-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "lineage-test-model")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // Create a key via the public endpoint so lineage_id ends up
    // self-referential (root invariant).
    let create_resp: Value = con
        .post(
            "/api/keys",
            json!({
                "name": "lineage-gateway-key",
                "surfaces": ["ai_gateway"]
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let plaintext = create_resp["key"].as_str().unwrap().to_string();
    let key_id_v1 = create_resp["id"].as_str().unwrap().to_string();
    let lineage_id: uuid::Uuid =
        sqlx::query_scalar("SELECT lineage_id FROM api_keys WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&key_id_v1).unwrap())
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(lineage_id.to_string(), key_id_v1);

    // Drive a request under generation 1.
    let gw = app.gateway_client();
    gw.set_bearer(&plaintext);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "lineage-test-model", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Wait for the row to land in CH with the lineage column set.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let n: u64 = ch
            .query("SELECT count() FROM gateway_logs WHERE api_key_lineage_id = ?")
            .bind(lineage_id.to_string())
            .fetch_one()
            .await
            .unwrap_or(0);
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Filter the read endpoint by lineage_id — expect ≥1 row.
    let body: Value = con
        .get(&format!(
            "/api/gateway/logs?api_key_lineage_id={lineage_id}&limit=50"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "lineage filter must return the row from generation 1: {body}"
    );
    for row in items {
        assert_eq!(
            row["api_key_lineage_id"].as_str(),
            Some(lineage_id.to_string().as_str()),
            "row must carry the same lineage_id we filtered on: {row}"
        );
    }
}
