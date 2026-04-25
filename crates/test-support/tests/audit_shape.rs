//! Audit-log shape contract for representative mutating handlers.
//!
//! Every mutating admin endpoint is supposed to emit exactly one
//! `audit_logs` row with a `verb.subject` action, the actor's
//! `user_id`, and a `resource` / `resource_id` that together
//! identify the row that changed. A future refactor that forgets
//! `state.audit.log(...)` is invisible at compile time and fails
//! silently in production — the actor's actions just stop showing
//! up in the audit explorer.
//!
//! Drive a representative sweep of user / api-key admin handlers
//! from one admin session, then poll ClickHouse for the expected
//! rows. Adding a new mutating endpoint? Add an `add(...)` call
//! after triggering it from `setup` — the assertion loop picks it
//! up automatically.
//!
//! Why focus on user / api-key admin paths and not providers /
//! mcp / log-forwarders? Those endpoints sit behind the SSRF guard
//! (`validate_url`) which rejects loopback / non-resolvable hosts —
//! the test environment doesn't have outbound DNS, so the create
//! call fails before the audit emit ever runs. Coverage for those
//! handlers' audit emits lives in their own per-feature tests
//! (`limits_override_meta.rs`, `webhook_outbox.rs`, …).
//!
//! `analytics_clickhouse.rs::audit_log_endpoint_lists_recent_entries`
//! covers the `auth.login` row + the read-side admin endpoint;
//! `limits_override_meta.rs` covers the `rate_limit.*` family.

use std::collections::BTreeMap;
use think_watch_test_support::prelude::*;

#[derive(Clone)]
struct ExpectedRow {
    action: &'static str,
    /// Substring that must appear inside `resource || resource_id`.
    /// Most handlers stamp `resource = "<kind>"` and
    /// `resource_id = <uuid>`, but several admin handlers use the
    /// shorter `resource = "<kind>:<id>"` form with `resource_id`
    /// empty — concatenate both before checking so either layout
    /// satisfies the contract.
    resource_contains: String,
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mutating_handlers_emit_one_audit_row_each() {
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

    let mut expected: BTreeMap<&'static str, ExpectedRow> = BTreeMap::new();
    let mut add = |row: ExpectedRow| {
        expected.insert(row.action, row);
    };

    // 1. Create a target user — `admin.create_user`.
    let new_email = format!("audit-{}@example.com", uuid::Uuid::new_v4().simple());
    let create_resp = con
        .post(
            "/api/admin/users",
            json!({
                "email": new_email,
                "display_name": "Audit Probe Target",
                "password": "Hunter2-strong-1!",
                "role_assignments": []
            }),
        )
        .await
        .unwrap();
    create_resp.assert_ok();
    let created: serde_json::Value = create_resp.json().unwrap();
    let target_id = created["id"]
        .as_str()
        .unwrap_or_else(|| panic!("create_user response missing `id`: {created:#}"))
        .to_string();
    add(ExpectedRow {
        action: "admin.create_user",
        resource_contains: target_id.clone(),
    });

    // 2. Update that user — `admin.update_user`.
    con.patch(
        &format!("/api/admin/users/{target_id}"),
        json!({"display_name": "Renamed by audit"}),
    )
    .await
    .unwrap()
    .assert_ok();
    add(ExpectedRow {
        action: "admin.update_user",
        resource_contains: target_id.clone(),
    });

    // 3. Reset that user's password — `admin.reset_password`. Uses
    //    `resource = "user:{id}"`, resource_id empty.
    con.post_empty(&format!("/api/admin/users/{target_id}/reset-password"))
        .await
        .unwrap()
        .assert_ok();
    add(ExpectedRow {
        action: "admin.reset_password",
        resource_contains: target_id.clone(),
    });

    // 4. Force-logout — `admin.force_logout`.
    con.post_empty(&format!("/api/admin/users/{target_id}/force-logout"))
        .await
        .unwrap()
        .assert_ok();
    add(ExpectedRow {
        action: "admin.force_logout",
        resource_contains: target_id.clone(),
    });

    // 5. Force-revoke an api key (created via fixture, no SSRF) —
    //    `api_key.force_revoke`.
    let key = fixtures::create_api_key(
        &app.db,
        uuid::Uuid::parse_str(&target_id).unwrap(),
        "audit-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    con.post(
        &format!("/api/admin/keys/{}/force-revoke", key.row.id),
        json!({"reason": "audit-shape probe"}),
    )
    .await
    .unwrap()
    .assert_ok();
    add(ExpectedRow {
        action: "api_key.force_revoke",
        resource_contains: key.row.id.to_string(),
    });

    // 6. Delete the user (last so prior rows aren't tombstoned by
    //    cascade) — `admin.delete_user`.
    con.delete(&format!("/api/admin/users/{target_id}"))
        .await
        .unwrap()
        .assert_ok();
    add(ExpectedRow {
        action: "admin.delete_user",
        resource_contains: target_id.clone(),
    });

    // -- Poll CH until every expected action lands. The audit
    //    pipeline batches with a flush interval of ~250ms; allow
    //    up to ~10s before failing.
    let ch = app.state.clickhouse.as_ref().expect("CH wired up");
    let admin_id = admin.user.id.to_string();
    type SeenRow = (Option<String>, Option<String>, Option<String>);
    let mut seen: BTreeMap<String, SeenRow> = BTreeMap::new();

    for _ in 0..200 {
        // Single-table query — every actor-attributed event lands in
        // `audit_logs`. The codebase used to split admin operations
        // into a separate `platform_logs` table; that split was
        // collapsed because the schemas were identical and the split
        // only fragmented the audit explorer.
        let rows: Vec<(String, String, String, String)> = ch
            .query(
                "SELECT action, ifNull(user_id, ''), ifNull(resource, ''), ifNull(resource_id, '') \
                   FROM audit_logs WHERE user_id = ? \
                 ORDER BY 1 LIMIT 400",
            )
            .bind(&admin_id)
            .fetch_all()
            .await
            .expect("CH query");
        for (action, uid, res, res_id) in rows {
            seen.entry(action)
                .or_insert((Some(uid), Some(res), Some(res_id)));
        }
        if expected.keys().all(|k| seen.contains_key(*k)) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Assert every expected action landed with the right shape.
    for (action, want) in &expected {
        let got = seen.get(*action).unwrap_or_else(|| {
            panic!(
                "audit row missing for action {action:?}; saw actions: {:?}",
                seen.keys().collect::<Vec<_>>()
            )
        });
        let (uid, res, res_id) = (got.0.as_deref(), got.1.as_deref(), got.2.as_deref());
        assert_eq!(
            uid,
            Some(admin_id.as_str()),
            "{action}: user_id must be the admin actor, got {uid:?}"
        );
        let resource_blob = format!("{}|{}", res.unwrap_or(""), res_id.unwrap_or(""));
        assert!(
            resource_blob.contains(&want.resource_contains),
            "{action}: resource/resource_id should contain {:?}, got {resource_blob:?}",
            want.resource_contains
        );
    }
}
