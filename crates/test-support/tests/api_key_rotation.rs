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
