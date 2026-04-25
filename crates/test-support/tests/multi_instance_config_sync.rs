//! Multi-instance config hot-reload via Redis Pub/Sub.
//!
//! In production, two backend instances pointing at the same
//! Postgres + Redis must stay in lock-step on `system_settings`
//! changes — instance B's in-memory `DynamicConfig` cache should
//! refresh within milliseconds of instance A persisting a write,
//! without restart. The mechanism: writer publishes `reload` to
//! `config:changed`; every subscriber re-reads the table.
//!
//! `dynamic_config.rs` has unit tests for `get`/`set` round-trips,
//! but nothing exercises the cross-instance pub/sub edge: a silent
//! regression that drops `notify_config_changed(...)` from the
//! update path, or a subscriber that fails to reload, would let
//! one instance serve stale config until the next restart.
//!
//! Test recipe: spawn TestApp (instance A's full stack), then
//! manually build a *second* `DynamicConfig` plus a Redis
//! `SubscriberClient` pointing at the same DB+Redis (instance B).
//! Drive a setting change through instance A's admin endpoint and
//! poll instance B's cache until it reflects the new value.

use std::sync::Arc;
use think_watch_common::dynamic_config::{DynamicConfig, spawn_config_subscriber};
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn instance_b_picks_up_instance_a_setting_change_via_pubsub() {
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

    // -- Stand up instance B's DynamicConfig cache + subscriber.
    //    Same DB pool (so it reads the same `system_settings` rows),
    //    independent Redis subscriber on the shared `config:changed`
    //    channel.
    let instance_b = Arc::new(
        DynamicConfig::load(app.db.clone())
            .await
            .expect("instance B DynamicConfig::load"),
    );
    let sub_cfg = fred::types::config::Config::from_url(&app.state.config.redis_url)
        .expect("parse redis_url");
    let subscriber: fred::clients::SubscriberClient = fred::types::Builder::from_config(sub_cfg)
        .build_subscriber_client()
        .expect("build_subscriber_client");
    fred::interfaces::ClientLike::init(&subscriber)
        .await
        .expect("subscriber init");
    spawn_config_subscriber(subscriber, instance_b.clone());

    // Tiny grace period for the SUBSCRIBE command to round-trip
    // before instance A publishes — without it the publish can
    // race ahead of the subscription confirmation and the message
    // is silently dropped (Redis pub/sub is fire-and-forget).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // -- Pick a setting key that already exists in `system_settings`
    //    (the migrations seed retention defaults). We bump it to a
    //    distinctive value so we can tell instance B saw THIS write
    //    and not stale data from a parallel test.
    let key = "data.retention_days_audit";
    let want = 17_i64; // intentionally not the default of 90.

    // -- Read instance B's `before` value so the assertion shows the
    //    delta on failure.
    let before = instance_b.get(key).await;

    // -- Drive the change through instance A's admin endpoint —
    //    this is what hits `notify_config_changed` in production.
    con.patch(
        "/api/admin/settings",
        json!({
            "settings": {
                key: want
            }
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // -- Poll instance B until it reloads. The pub/sub round-trip
    //    is single-digit ms locally; allow up to 5s for slow CI.
    let mut got: Option<serde_json::Value> = None;
    for _ in 0..100 {
        let v = instance_b.get(key).await;
        if v.as_ref().and_then(|j| j.as_i64()) == Some(want) {
            got = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        got.as_ref().and_then(|j| j.as_i64()),
        Some(want),
        "instance B never observed the new value: before={before:?} now={got:?}"
    );
}
