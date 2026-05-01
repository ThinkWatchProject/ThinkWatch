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
async fn gateway_logs_filter_by_api_key_id_rolls_up_history_after_rotation() {
    // The whole point of `lineage_id` from the operator's POV:
    // querying `?api_key_id=<gen-1>` after the key has been rotated
    // returns the rows from BOTH generation 1 (driven below) AND
    // generation 2 (also driven below). The handler resolves
    // api_key_id → lineage_id internally and filters CH on the
    // lineage column, so the caller never has to know about
    // rotation generations.
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

    // Generation 1.
    let create_resp: Value = con
        .post(
            "/api/keys",
            json!({"name": "lineage-gateway-key", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let plaintext_v1 = create_resp["key"].as_str().unwrap().to_string();
    let key_id_v1 = create_resp["id"].as_str().unwrap().to_string();

    // Drive 1 request under gen 1.
    let gw = app.gateway_client();
    gw.set_bearer(&plaintext_v1);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "lineage-test-model", "messages": [{"role": "user", "content": "gen1"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Rotate → generation 2.
    let rotated: Value = con
        .post_empty(&format!("/api/keys/{key_id_v1}/rotate"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let plaintext_v2 = rotated["key"].as_str().unwrap().to_string();

    // Drive 1 request under gen 2.
    let gw2 = app.gateway_client();
    gw2.set_bearer(&plaintext_v2);
    gw2.post(
        "/v1/chat/completions",
        json!({"model": "lineage-test-model", "messages": [{"role": "user", "content": "gen2"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Wait for both rows to land in CH.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let n: u64 = ch
            .query("SELECT count() FROM gateway_logs WHERE cost_usd IS NOT NULL")
            .fetch_one()
            .await
            .unwrap_or(0);
        if n >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Filter the read endpoint by the GENERATION-1 api_key_id —
    // the handler must transparently resolve to lineage and return
    // BOTH generations' rows.
    let body: Value = con
        .get(&format!(
            "/api/gateway/logs?api_key_id={key_id_v1}&limit=50"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "rotation-transparent filter must return both gen-1 + gen-2 rows: {body}"
    );

    // The two rows have DIFFERENT api_key_ids (one per generation),
    // proving the rollup wasn't just an exact-match filter.
    let ids: std::collections::HashSet<&str> = items
        .iter()
        .filter_map(|r| r["api_key_id"].as_str())
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "rolled-up rows must come from 2 distinct api_key_ids (the rotation chain): {ids:?}"
    );

    // The response must NOT leak `api_key_lineage_id` to the client
    // — lineage is a server-internal concept now.
    assert!(
        items[0].get("api_key_lineage_id").is_none(),
        "api_key_lineage_id must not appear in the response: {}",
        items[0]
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rotating_an_already_rotated_key_returns_400() {
    // A rotated key keeps `is_active = true` during the grace window
    // (so existing clients can finish hot-swapping). The original
    // `is_active`-only guard let users hit the rotate endpoint a
    // second time on the same row, orphaning gen-2 in the chain
    // (gen-1 → gen-3 with rotated_from_id pointing at gen-1, while
    // gen-2 sits between them) and silently extending gen-1's grace
    // period. Both effects are now blocked by checking
    // `disabled_reason = 'rotated'` explicitly.
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
            json!({"name": "Re-rotate Guard", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let id = create["id"].as_str().unwrap().to_string();

    // First rotation succeeds.
    con.post_empty(&format!("/api/keys/{id}/rotate"))
        .await
        .unwrap()
        .assert_ok();

    // Second rotation on the SAME (now-rotated) row must 400 — the
    // grace-period guard short-circuits before any DB writes happen.
    con.post_empty(&format!("/api/keys/{id}/rotate"))
        .await
        .unwrap()
        .assert_status(400);

    // Side-effect contract: the second attempt must NOT have created
    // a third row in the lineage. Exactly two rows: the original (now
    // disabled_reason='rotated') and the new key.
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE deleted_at IS NULL AND name = $1")
            .bind("Re-rotate Guard")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        count.0, 2,
        "blocked re-rotation must not insert a third generation"
    );
}
