//! The gateway's Lua scripts and multi-key commands against a real
//! Redis Cluster.
//!
//! A cluster runs a script only when every key it declares hashes to one
//! slot, refuses a `DEL` across slots (`CROSSSLOT`), and answers `SCAN`
//! for one node only. These tests point at a cluster given by
//! `TEST_REDIS_CLUSTER_URL` (e.g. `redis-cluster://127.0.0.1:37001`) and
//! skip without it, so CI — which has a single Redis — stays green. To
//! run them locally, start a three-primary cluster whose nodes announce
//! an address this process can reach, for example:
//!
//! ```text
//! docker run -d --rm --name tw-redis-cluster \
//!   -p 37001:37001 -p 37002:37002 -p 37003:37003 redis:8-alpine sh -c '
//!   for p in 37001 37002 37003; do
//!     redis-server --port $p --cluster-enabled yes --cluster-config-file n-$p.conf \
//!       --cluster-announce-ip 127.0.0.1 --protected-mode no --save "" --daemonize yes --dir /tmp
//!   done; sleep 1
//!   redis-cli --cluster create 127.0.0.1:37001 127.0.0.1:37002 127.0.0.1:37003 \
//!     --cluster-replicas 0 --cluster-yes; tail -f /dev/null'
//! TEST_REDIS_CLUSTER_URL=redis-cluster://127.0.0.1:37001 \
//!   cargo nextest run -p think-watch-test-support --test redis_cluster --run-ignored only
//! ```
//!
//! Keys carry fresh UUIDs, so nothing is flushed and runs don't collide.
//!
//! A TLS cluster (`rediss-cluster://`) works the same way, with
//! `TEST_REDIS_CA_CERT` naming the CA of its certificates; see
//! `tests/redis_tls.rs` for how to start one.

use fred::clients::Client;
use fred::interfaces::{ClientLike, KeysInterface};
use fred::types::Builder;
use think_watch_test_support::prelude::*;
use think_watch_test_support::test_redis_config;

fn cluster_url() -> Option<String> {
    let url = std::env::var("TEST_REDIS_CLUSTER_URL").ok();
    if url.is_none() {
        eprintln!("TEST_REDIS_CLUSTER_URL not set — skipping the Redis Cluster test");
    }
    url
}

async fn cluster(url: &str) -> Client {
    let client = Builder::from_config(test_redis_config(url))
        .build()
        .unwrap();
    client.init().await.unwrap();
    assert!(client.is_clustered(), "{url} is not a cluster URL");
    client
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn the_limit_scripts_run_on_a_cluster() {
    let Some(url) = cluster_url() else { return };
    let redis = cluster(&url).await;
    think_watch_test_support::redis_scripts::exercise_the_limit_scripts(&redis).await;
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn budgets_and_route_health_run_on_a_cluster() {
    use think_watch_common::limits::{BudgetCap, BudgetPeriod, BudgetSubject, budget};
    use think_watch_gateway::health::{CircuitBreakerConfig, HealthTracker};

    let Some(url) = cluster_url() else { return };
    let redis = cluster(&url).await;

    // Budgets: one key per command, any slot.
    let cap = |kind, id| BudgetCap {
        id: Uuid::nil(),
        subject_kind: kind,
        subject_id: id,
        period: BudgetPeriod::Daily,
        limit_tokens: 100,
        enabled: true,
        expires_at: None,
        reason: None,
        created_by: None,
    };
    let caps = [
        cap(BudgetSubject::User, Uuid::new_v4()),
        cap(BudgetSubject::ApiKeyLineage, Uuid::new_v4()),
    ];
    budget::add_weighted_tokens(&redis, &caps, 60)
        .await
        .unwrap();
    let (statuses, crossings) = budget::add_weighted_tokens(&redis, &caps, 60)
        .await
        .unwrap();
    assert_eq!(
        statuses.iter().map(|s| s.current).collect::<Vec<_>>(),
        [120, 120]
    );
    assert_eq!(crossings.len(), 6, "80/95/100 % on both caps");
    let spent = budget::current_spend(&redis, &caps).await.unwrap();
    assert_eq!(
        spent.iter().map(|s| s.current).collect::<Vec<_>>(),
        [120, 120]
    );

    // Route health: three keys in one script, then two, then a DEL of
    // all three.
    let health = HealthTracker::new(redis.clone());
    let route = Uuid::new_v4();
    let cfg = CircuitBreakerConfig {
        enabled: true,
        error_pct: 50,
        min_samples: 2,
        window_secs: 60,
        open_secs: 30,
    };
    health.record(route, 10, true, cfg).await;
    let after = health.record(route, 10, true, cfg).await;
    assert_eq!(after.total, 2, "the record script ran");
    assert_eq!(after.lifetime_requests, 2);
    assert_eq!(format!("{:?}", after.state), "Open", "the state write ran");
    assert_eq!(format!("{:?}", health.state(route, cfg).await), "Open");
    health.forget(route).await;
    assert_eq!(health.snapshot(route, cfg).await.lifetime_requests, 0);
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn the_route_cap_scripts_run_on_a_cluster() {
    let Some(url) = cluster_url() else { return };
    let redis = cluster(&url).await;
    think_watch_test_support::redis_scripts::exercise_the_route_cap_scripts(&redis).await;
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn a_pattern_delete_reaches_every_node_of_a_cluster() {
    let Some(url) = cluster_url() else { return };
    let redis = cluster(&url).await;

    let run = Uuid::new_v4();
    let keys: Vec<String> = (0..300).map(|i| format!("tw-test:{run}:{i}")).collect();
    for k in &keys {
        let _: () = redis.set(k, 1, None, None, false).await.unwrap();
    }
    let slots: std::collections::HashSet<u16> = keys
        .iter()
        .map(|k| fred::util::redis_keyslot(k.as_bytes()))
        .collect();
    assert!(slots.len() > 100, "the keys spread over the cluster");

    let deleted =
        think_watch_common::redis_keys::delete_matching(&redis, &format!("tw-test:{run}:*"), None)
            .await
            .unwrap();
    assert_eq!(deleted, keys.len());
    for k in &keys {
        assert_eq!(redis.exists::<i64, _>(k).await.unwrap(), 0, "{k}");
    }
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn the_gateway_enforces_limits_on_a_cluster() {
    let Some(url) = cluster_url() else { return };
    let app = TestApp::try_spawn_with(SpawnOptions {
        redis_url: Some(url),
        ..Default::default()
    })
    .await
    .unwrap();

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let user_id = user.user.id;
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("cluster-prov"),
        "openai",
        &mock.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("cluster-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    // The user's request limit and the key's token limit and budget: one
    // script checks counters of both subjects.
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 2)
        .await
        .unwrap();
    fixtures::create_rate_limit_rule(
        &app.db,
        "api_key_lineage",
        key.row.lineage_id,
        "ai_gateway",
        "tokens",
        60,
        1_000_000,
    )
    .await
    .unwrap();
    fixtures::create_budget_cap(
        &app.db,
        "api_key_lineage",
        key.row.lineage_id,
        "daily",
        1_000_000,
    )
    .await
    .unwrap();
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    // A prompt of its own: the response cache lives in the same Redis,
    // which nothing flushes, and a cached answer records no tokens.
    let chat = json!({"model": "gpt-test",
                      "messages": [{"role": "user", "content": Uuid::new_v4().to_string()}]});
    for _ in 0..2 {
        gw.post("/v1/chat/completions", chat.clone())
            .await
            .unwrap()
            .assert_ok();
    }
    let r = gw.post("/v1/chat/completions", chat).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    assert!(r.headers.get("retry-after").is_some());

    let usage: Json = con
        .get(&format!("/api/admin/limits/api_key/{}/usage", key.row.id))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(
        usage["rules"][0]["current"].as_i64().unwrap() > 0,
        "the key's tokens were recorded: {usage}"
    );
    assert!(
        usage["caps"][0]["current"].as_i64().unwrap() > 0,
        "the key's budget was debited: {usage}"
    );
    let usage: Json = con
        .get(&format!("/api/admin/limits/user/{user_id}/usage"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(usage["rules"][0]["current"], 2, "{usage}");

    // The key's and its owner's day and month usage: one script over four
    // hashes (the owner's hash tag), read back by `GET /v1/usage`. The
    // refused request counted nothing.
    let reported: Json = gw.get("/v1/usage").await.unwrap().json().unwrap();
    assert_eq!(reported["scope"], "key", "{reported}");
    assert_eq!(reported["usage"]["requests_today"], 2, "{reported}");
    assert_eq!(reported["usage"]["requests_month"], 2, "{reported}");
    assert!(
        reported["usage"]["tokens_today"].as_i64().unwrap() > 0,
        "the key's tokens were counted: {reported}"
    );
}

#[ignore = "integration test — needs TEST_REDIS_CLUSTER_URL"]
#[tokio::test]
async fn config_change_notices_reach_a_subscriber_on_a_cluster() {
    // What `init::spawn_config_subscriber` does with the same URL: a
    // subscriber on one node hears a publish sent through another.
    let Some(url) = cluster_url() else { return };
    let publisher = cluster(&url).await;
    think_watch_test_support::assert_config_notice_arrives(test_redis_config(&url), &publisher)
        .await;
}
