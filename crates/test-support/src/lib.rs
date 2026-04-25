//! Test harness for ThinkWatch integration tests.
//!
//! [`TestApp::spawn`] boots the full server stack (gateway + console
//! routers) in-process against a freshly-created Postgres database
//! and the local Redis instance, returning the URLs to hit and a
//! signing-aware [`TestClient`]. Each test gets its own isolated DB.
//!
//! Required env (defaults match `make infra`):
//! - `TEST_DATABASE_BASE_URL` (default `postgres://thinkwatch:thinkwatch@localhost:5432`)
//! - `TEST_REDIS_URL` (default `redis://localhost:6379`)
//!
//! Tests run against the same Redis; isolation is achieved by giving
//! every fixture a fresh UUID-suffixed email / user id, which ensures
//! the rate-limit, lockout, and signing keys never collide.

pub mod ch;
pub mod client;
pub mod fixtures;
pub mod mock_provider;
pub mod pg;

use std::sync::Arc;

use anyhow::Context;
use fred::clients::Client as RedisClient;
use fred::interfaces::ClientLike;
use fred::types::Builder;
use fred::types::config::Config as RedisConfig;
use sqlx::PgPool;
use think_watch_common::config::AppConfig;
use think_watch_server::{app, init};
use tokio::sync::oneshot;

pub use ch::IsolatedClickHouseDatabase;
pub use client::{SignedKey, TestClient};
pub use mock_provider::MockProvider;
pub use pg::IsolatedDatabase;

/// A fully-booted ThinkWatch server in-process. Holds:
///  * the per-test Postgres database (dropped on shutdown)
///  * the redis client used by the server
///  * the gateway and console listening sockets
///  * the [`AppState`] so tests can prod the lower-level objects
///    (audit logger, dynamic config, mcp registry, …) directly.
pub struct TestApp {
    pub gateway_url: String,
    pub console_url: String,
    pub state: think_watch_server::app::AppState,
    pub db: PgPool,
    /// Holds the per-test database alive — drops trigger DROP DATABASE.
    #[allow(dead_code)]
    db_owner: IsolatedDatabase,
    /// Per-test ClickHouse database, present only when the test
    /// opted into CH via `spawn_with_clickhouse`. Drop runs DROP
    /// DATABASE.
    #[allow(dead_code)]
    ch_owner: Option<IsolatedClickHouseDatabase>,
    shutdown: Option<oneshot::Sender<()>>,
    join_gateway: Option<tokio::task::JoinHandle<()>>,
    join_console: Option<tokio::task::JoinHandle<()>>,
}

/// Knobs for `TestApp::spawn_with`. Default = no ClickHouse, no
/// background loops, no CB listener — same as `TestApp::spawn`.
#[derive(Default, Clone)]
pub struct SpawnOptions {
    /// When true, create a per-test ClickHouse database, run the
    /// schema bootstrap, and pass the wired-up client to
    /// `init_state` so the audit pipeline writes there.
    pub clickhouse: bool,
}

impl TestApp {
    /// Boot a fresh `TestApp`. Panics on failure — fail-fast in tests
    /// is the right call.
    pub async fn spawn() -> Self {
        Self::try_spawn().await.expect("TestApp::spawn failed")
    }

    /// Same as [`spawn`] but opts the test into a per-test ClickHouse
    /// database. Use when the test asserts on `gateway_logs`,
    /// `audit_logs`, the analytics endpoints, or anything else that
    /// reads back what the audit pipeline wrote.
    pub async fn spawn_with_clickhouse() -> Self {
        Self::try_spawn_with(SpawnOptions { clickhouse: true })
            .await
            .expect("TestApp::spawn_with_clickhouse failed")
    }

    pub async fn try_spawn() -> anyhow::Result<Self> {
        Self::try_spawn_with(SpawnOptions::default()).await
    }

    pub async fn try_spawn_with(opts: SpawnOptions) -> anyhow::Result<Self> {
        init_test_tracing();

        let base_url = std::env::var("TEST_DATABASE_BASE_URL").unwrap_or_else(|_| {
            "postgres://thinkwatch:7c3fe6307d00fe3f2f29f534e806ac71@localhost:5432".into()
        });
        // Default to logical DB 1 so we never trample the dev Redis
        // (DB 0). Override via env when CI uses a dedicated instance.
        let redis_url = std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| {
            "redis://:225b3facaf55212ff86ad6595e6d6471@localhost:6379/1".into()
        });

        // Per-test database with migrations applied.
        let db_owner = IsolatedDatabase::create(&base_url)
            .await
            .context("create per-test database")?;
        let db = db_owner.pool().clone();

        // Redis: shared instance on a dedicated logical DB. We
        // FLUSHDB at spawn time to clear any stragglers from prior
        // tests. Tests must therefore run serially
        // (`--test-threads=1`) — the Makefile target enforces it.
        let redis = build_redis(&redis_url).await?;
        // fred 10 doesn't expose FLUSHDB directly (only FLUSHALL),
        // and we don't want to nuke the dev DB. Send the raw
        // command so we only clear the test logical DB.
        {
            use fred::interfaces::ClientLike;
            use fred::types::{ClusterHash, CustomCommand};
            let cmd = CustomCommand::new("FLUSHDB", ClusterHash::FirstKey, false);
            let _: () = redis
                .custom(cmd, Vec::<String>::new())
                .await
                .context("FLUSHDB on test redis DB")?;
        }

        // Per-test ClickHouse — only when the test asked for it.
        let (ch_owner, ch_client, ch_url, ch_db, ch_user, ch_password) = if opts.clickhouse {
            let url = std::env::var("TEST_CLICKHOUSE_URL")
                .unwrap_or_else(|_| "http://localhost:8123".into());
            let user = std::env::var("TEST_CLICKHOUSE_USER")
                .ok()
                .or_else(|| Some("thinkwatch".into()));
            let password = std::env::var("TEST_CLICKHOUSE_PASSWORD")
                .ok()
                .or_else(|| Some("c693ded3da8388c7b6a4288dac91a2ad".into()));
            let owner =
                IsolatedClickHouseDatabase::create(&url, user.as_deref(), password.as_deref())
                    .await
                    .context("create per-test ClickHouse database")?;
            let db_name = owner.database().to_string();
            let client = owner.client().clone();
            (
                Some(owner),
                Some(client),
                Some(url),
                db_name,
                user,
                password,
            )
        } else {
            (None, None, None, "test".to_string(), None, None)
        };

        let config = AppConfig {
            database_url: db_owner.url().to_string(),
            redis_url,
            jwt_secret: "test-jwt-secret-with-enough-entropy-aaa".into(),
            // 64 hex chars = 32 bytes; valid for AES-256-GCM.
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            server_host: "127.0.0.1".into(),
            gateway_port: 0,
            console_port: 0,
            cors_origins: vec!["http://localhost".into()],
            clickhouse_url: ch_url,
            clickhouse_db: ch_db,
            clickhouse_user: ch_user,
            clickhouse_password: ch_password,
            metrics_bearer_token: None,
        };
        config.validate().map_err(anyhow::Error::msg)?;

        let state = init::init_state(config.clone(), db.clone(), redis, ch_client).await?;

        // We deliberately skip:
        //   * install_cb_listener (process-global OnceLock)
        //   * spawn_config_subscriber (would subscribe to a shared
        //     Redis channel; tests should drive config changes
        //     synchronously)
        //   * spawn_background_tasks (tests want deterministic timing
        //     — call the underlying functions directly when needed)

        let gateway_router = app::create_gateway_app(&config, state.clone()).await?;
        let console_router = app::create_console_app(&config, state.clone())?;

        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let console_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let gateway_addr = gateway_listener.local_addr()?;
        let console_addr = console_listener.local_addr()?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (gw_signal_tx, gw_signal_rx) = oneshot::channel::<()>();
        let (con_signal_tx, con_signal_rx) = oneshot::channel::<()>();

        // Fan-out: one Drop signal triggers both servers' graceful
        // shutdown.
        tokio::spawn(async move {
            let _ = shutdown_rx.await;
            let _ = gw_signal_tx.send(());
            let _ = con_signal_tx.send(());
        });

        let join_gateway = tokio::spawn(async move {
            let svc = gateway_router.into_make_service_with_connect_info::<std::net::SocketAddr>();
            let _ = axum::serve(gateway_listener, svc)
                .with_graceful_shutdown(async move {
                    let _ = gw_signal_rx.await;
                })
                .await;
        });
        let join_console = tokio::spawn(async move {
            let svc = console_router.into_make_service_with_connect_info::<std::net::SocketAddr>();
            let _ = axum::serve(console_listener, svc)
                .with_graceful_shutdown(async move {
                    let _ = con_signal_rx.await;
                })
                .await;
        });

        Ok(TestApp {
            gateway_url: format!("http://{gateway_addr}"),
            console_url: format!("http://{console_addr}"),
            state,
            db,
            db_owner,
            ch_owner,
            shutdown: Some(shutdown_tx),
            join_gateway: Some(join_gateway),
            join_console: Some(join_console),
        })
    }

    /// Build a `TestClient` rooted at the console URL.
    pub fn console_client(&self) -> TestClient {
        TestClient::new(self.console_url.clone())
    }

    /// Build a `TestClient` rooted at the gateway URL.
    pub fn gateway_client(&self) -> TestClient {
        TestClient::new(self.gateway_url.clone())
    }

    /// Force a reload of the gateway model router. Call after
    /// inserting / mutating providers or model_routes via the test
    /// fixture helpers so the in-memory router sees them.
    pub async fn rebuild_gateway_router(&self) {
        app::rebuild_gateway_router(&self.state).await;
    }

    /// Explicit teardown. Optional — `Drop` runs the same path.
    pub async fn shutdown(mut self) {
        self.shutdown_inner();
        if let Some(handle) = self.join_gateway.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.join_console.take() {
            let _ = handle.await;
        }
    }

    fn shutdown_inner(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

async fn build_redis(redis_url: &str) -> anyhow::Result<RedisClient> {
    let cfg = RedisConfig::from_url(redis_url).context("parse REDIS_URL")?;
    let client = Builder::from_config(cfg).build()?;
    client.init().await.context("redis init")?;
    Ok(client)
}

fn init_test_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Default to ERROR — tests don't want noisy output. Override
        // with `RUST_LOG=think_watch=debug` when debugging.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    });
}

/// Convenience re-exports so test files only need one `use`.
pub mod prelude {
    pub use crate::TestApp;
    pub use crate::client::{SignedKey, TestClient};
    pub use crate::fixtures;
    pub use crate::mock_provider::MockProvider;
    pub use serde_json::{Value as Json, json};
    pub use uuid::Uuid;

    /// Generate a unique-per-test email so Redis lockout / rate-limit
    /// keys (which are keyed on email) don't collide across parallel
    /// tests sharing a Redis instance.
    pub fn unique_email() -> String {
        format!("test-{}@example.com", Uuid::new_v4().simple())
    }

    /// Builder for a unique team / role / api-key / etc. name.
    pub fn unique_name(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4().simple())
    }
}

/// Re-export `Arc` so test crates don't have to add std imports.
pub use std::sync::Arc as TestArc;

/// Internal helper used by the spawn path.
fn _strip_arc<T>(arc: Arc<T>) -> Arc<T> {
    arc
}
