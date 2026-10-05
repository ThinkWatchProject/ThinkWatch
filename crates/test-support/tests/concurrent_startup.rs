//! Several instances starting at the same moment against one database:
//! a Helm `replicaCount` above 1, a rolling upgrade, an autoscaler adding
//! pods. Every instance applies `db/schema.sql`, the seeds and the
//! guard-settings conversion, then sets up the ClickHouse tables and
//! backfills the rollups. Run side by side, the schema statements of one
//! instance deadlocked with another's, and one of them exited at its
//! migration; two backfills of the same empty rollup each copied the
//! whole log into it.
//!
//! Each round here starts the setup on [`INSTANCES`] tasks at once, each
//! with a connection pool of its own as separate processes would have:
//! every one succeeds, and the database ends up as one instance alone
//! leaves it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use think_watch_common::db::{SCHEMA_LOCK, StartupLock, run_migrations};
use think_watch_test_support::{
    IsolatedClickHouseDatabase, IsolatedDatabase, clickhouse_env, database_base_url,
};
use tokio::sync::Barrier;

const INSTANCES: usize = 4;

/// One pool per instance, against the same database.
async fn instance_pools(db: &IsolatedDatabase) -> Vec<PgPool> {
    let mut pools = Vec::with_capacity(INSTANCES);
    for _ in 0..INSTANCES {
        pools.push(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(db.url())
                .await
                .expect("connect an instance's pool"),
        );
    }
    pools
}

/// Start the schema setup on every instance at the same moment and
/// return what each one returned.
async fn migrate_at_once(db: &IsolatedDatabase) -> Vec<Result<(), String>> {
    let barrier = Arc::new(Barrier::new(INSTANCES));
    let mut tasks = Vec::with_capacity(INSTANCES);
    for pool in instance_pools(db).await {
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = run_migrations(&pool).await.map_err(|e| format!("{e:#}"));
            pool.close().await;
            result
        }));
    }
    let mut results = Vec::with_capacity(INSTANCES);
    for task in tasks {
        results.push(task.await.expect("an instance's setup panicked"));
    }
    results
}

fn assert_all_started(results: &[Result<(), String>]) {
    let failed: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        failed.is_empty(),
        "{} of {INSTANCES} instances failed their migration: {failed:#?}",
        failed.len()
    );
}

/// Every object the schema setup creates, one line each, sorted.
async fn schema_of(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT 'column ' || table_name || '.' || column_name || ' ' || data_type
                || ' nullable=' || is_nullable || ' default=' || coalesce(column_default, '')
           FROM information_schema.columns WHERE table_schema = 'public'
         UNION ALL
         SELECT 'index ' || indexdef FROM pg_indexes WHERE schemaname = 'public'
         UNION ALL
         SELECT 'constraint ' || conrelid::regclass::text || ' ' || conname || ' '
                || pg_get_constraintdef(oid)
           FROM pg_constraint WHERE connamespace = 'public'::regnamespace
         UNION ALL
         SELECT 'trigger ' || pg_get_triggerdef(oid) FROM pg_trigger WHERE NOT tgisinternal
         UNION ALL
         SELECT 'function ' || oid::regprocedure::text
           FROM pg_proc WHERE pronamespace = 'public'::regnamespace
         UNION ALL
         SELECT 'extension ' || extname FROM pg_extension
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("read the schema")
}

/// Every row the seeds write, one line each, sorted. The conversion's
/// record says when it was written, so only its presence is compared.
async fn seeds_of(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT 'role ' || name || ' ' || policy_document::text FROM rbac_roles
         UNION ALL
         SELECT 'surface ' || name FROM api_key_surface_kinds
         UNION ALL
         SELECT 'pricing ' || id::text FROM platform_pricing
         UNION ALL
         SELECT 'setting ' || key || ' '
                || CASE WHEN key = 'security.legacy_converted' THEN 'present' ELSE value::text END
           FROM system_settings
         UNION ALL
         SELECT 'template ' || slug FROM mcp_store_templates
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("read the seeded rows")
}

async fn setting(pool: &PgPool, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_starting_together_on_a_fresh_database_all_start() {
    let base = database_base_url();
    let alone = IsolatedDatabase::create(&base).await.unwrap();
    let db = IsolatedDatabase::create_empty(&base).await.unwrap();

    // The first start of a deployment: an empty database.
    assert_all_started(&migrate_at_once(&db).await);
    assert_eq!(schema_of(db.pool()).await, schema_of(alone.pool()).await);
    assert_eq!(seeds_of(db.pool()).await, seeds_of(alone.pool()).await);

    // All of them restarting at once (a rollout of the same version):
    // everything is there already, and nothing changes.
    assert_all_started(&migrate_at_once(&db).await);
    assert_eq!(schema_of(db.pool()).await, schema_of(alone.pool()).await);
    assert_eq!(seeds_of(db.pool()).await, seeds_of(alone.pool()).await);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_upgrading_together_apply_the_upgrade_once() {
    let base = database_base_url();
    let alone = IsolatedDatabase::create(&base).await.unwrap();
    let db = IsolatedDatabase::create(&base).await.unwrap();

    // The database as an earlier version leaves it: columns and an index
    // the schema has added since are missing, and the guard settings are
    // still the old ones, with nothing converted yet.
    sqlx::raw_sql(
        r#"ALTER TABLE models DROP COLUMN max_output_tokens;
           ALTER TABLE model_routes DROP COLUMN upstream_protocol;
           DROP INDEX idx_api_keys_user_not_deleted;
           DELETE FROM system_settings WHERE key = 'security.legacy_converted';
           INSERT INTO system_settings (key, value, category, description) VALUES
               ('security.hidden_text', '"block"', 'security', 'old'),
               ('security.tool_inspection',
                '{"mode": "enforce", "disabled": ["chmod-777"], "actions": {}, "custom": []}',
                'security', 'old');
           ALTER TABLE models ADD COLUMN output_guardrails JSONB NOT NULL DEFAULT '[]'::jsonb;"#,
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert_all_started(&migrate_at_once(&db).await);

    // The schema is complete, the old column is gone...
    assert_eq!(schema_of(db.pool()).await, schema_of(alone.pool()).await);
    // ...and the old settings were converted, by one of them.
    for key in ["security.hidden_text", "security.tool_inspection"] {
        assert_eq!(setting(db.pool(), key).await, None, "{key}");
    }
    let marker = setting(db.pool(), "security.legacy_converted")
        .await
        .expect("the conversion was recorded");
    let mut converted: Vec<&str> = marker["converted"]
        .as_array()
        .expect("{marker}")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    converted.sort_unstable();
    assert_eq!(
        converted,
        ["security.hidden_text", "security.tool_inspection"],
        "{marker}"
    );
    assert_eq!(
        setting(db.pool(), "security.inspect_tools").await,
        Some(serde_json::json!({"mode": "enforce", "disable": ["chmod-777"]})),
    );
}

/// Advisory locks held in `pool`'s database: `(granted, waiting)`.
async fn advisory_locks(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE granted), count(*) FILTER (WHERE NOT granted)
           FROM pg_locks
          WHERE locktype = 'advisory'
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Wait until `pool`'s database shows `want` advisory locks.
async fn until_advisory_locks(pool: &PgPool, want: (i64, i64), what: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while advisory_locks(pool).await != want {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: the advisory locks never came to {want:?}"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_schema_lock_never_outlives_its_holder() {
    let db = IsolatedDatabase::create(&database_base_url())
        .await
        .unwrap();

    // An instance that dies holding the lock: its session ends, and the
    // instance waiting for it goes ahead.
    let mut holder = StartupLock::acquire(db.pool(), SCHEMA_LOCK, "testing")
        .await
        .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(holder.conn())
        .await
        .unwrap();
    let pool = db.pool().clone();
    let waiter = tokio::spawn(async move { run_migrations(&pool).await });
    until_advisory_locks(db.pool(), (1, 1), "the second instance waits").await;
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(db.pool())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("the waiting instance got the lock once its holder was gone")
        .unwrap()
        .unwrap();
    drop(holder);
    until_advisory_locks(db.pool(), (0, 0), "released after the migration").await;

    // A setup that fails drops the lock without releasing it: its
    // connection is closed, not handed back to the pool still holding it.
    let lock = StartupLock::acquire(db.pool(), SCHEMA_LOCK, "testing")
        .await
        .unwrap();
    assert_eq!(advisory_locks(db.pool()).await, (1, 0));
    drop(lock);
    until_advisory_locks(db.pool(), (0, 0), "released on drop").await;
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_starting_together_backfill_the_rollups_once() {
    let pg = IsolatedDatabase::create(&database_base_url())
        .await
        .unwrap();
    let (url, user, password) = clickhouse_env();
    let ch = IsolatedClickHouseDatabase::create(&url, user.as_deref(), password.as_deref())
        .await
        .unwrap();
    let client = ch.client().clone();

    // Requests logged before the rollups existed: the rows are in
    // gateway_logs, and the rollups a new version adds start empty.
    client
        .query(
            "INSERT INTO gateway_logs (id, model_id, provider, status_code, latency_ms, \
                                       input_tokens, output_tokens) \
             SELECT toString(number), 'm', 'p', 200, 10, 3, 4 FROM numbers(25)",
        )
        .execute()
        .await
        .unwrap();
    for table in ["cost_rollup_hourly", "provider_health_5m"] {
        client
            .query(&format!("TRUNCATE TABLE {table}"))
            .execute()
            .await
            .unwrap();
    }

    let barrier = Arc::new(Barrier::new(INSTANCES));
    let mut tasks = Vec::with_capacity(INSTANCES);
    for pool in instance_pools(&pg).await {
        let barrier = barrier.clone();
        let ch = Some(client.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = think_watch_server::init::ensure_clickhouse_schema(&pool, &ch)
                .await
                .map_err(|e| format!("{e:#}"));
            pool.close().await;
            result
        }));
    }
    let mut results = Vec::with_capacity(INSTANCES);
    for task in tasks {
        results.push(task.await.expect("an instance's setup panicked"));
    }
    assert_all_started(&results);

    let requests: u64 = client
        .query("SELECT toUInt64(sum(request_count)) FROM cost_rollup_hourly")
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(requests, 25, "cost_rollup_hourly counts each request once");
    let tokens: i64 = client
        .query("SELECT toInt64(sum(output_tokens)) FROM cost_rollup_hourly")
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(tokens, 100);
    let health: u64 = client
        .query("SELECT toUInt64(sum(total_requests)) FROM provider_health_5m")
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(health, 25, "provider_health_5m counts each request once");
}
