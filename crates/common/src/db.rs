use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgConnection, PgPool, Postgres};

pub async fn create_pool(database_url: &str) -> anyhow::Result<PgPool> {
    if !database_url.contains("sslmode=") {
        tracing::warn!("DATABASE_URL does not specify sslmode. Use sslmode=require in production.");
    }

    // 80 keeps headroom for ~50 concurrent active users (each request
    // typically issues 2-3 queries through the auth + RBAC + handler
    // path). Bump to 120 if QPS sustains over ~500. Override per-env
    // with DB_MAX_CONNECTIONS without touching code.
    let max_connections: u32 = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80);

    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(database_url)
        .await?;

    // sqlx emits query timings via tracing when RUST_LOG includes sqlx=info.
    // For slow-query visibility, set RUST_LOG=sqlx::query=warn to see queries > 1s.

    tracing::info!(max_connections, "Database connection pool created");
    Ok(pool)
}

/// Advisory-lock key held while an instance applies the schema, the
/// seeds and the conversions ([`run_migrations`]): "twschema" in ASCII.
pub const SCHEMA_LOCK: i64 = i64::from_be_bytes(*b"twschema");

/// Advisory-lock key held while an instance sets up the ClickHouse tables
/// and backfills the rollups: "twchinit" in ASCII.
pub const CLICKHOUSE_SETUP_LOCK: i64 = i64::from_be_bytes(*b"twchinit");

/// A Postgres session-level advisory lock on a connection of its own.
///
/// Start-up work that every instance runs, but that two instances must
/// not run at once, holds one: an instance that starts while another
/// holds the lock waits for it, then finds the work done.
///
/// The lock lasts as long as the session. The connection never goes back
/// to the pool — [`release`](Self::release) closes it, and so does
/// dropping the guard on an error — so no later query can find itself
/// holding the lock. An instance that dies holding it loses its
/// connection, and Postgres releases the lock with the session.
pub struct StartupLock {
    conn: PoolConnection<Postgres>,
}

impl StartupLock {
    /// Take the lock `key`, waiting while another instance holds it.
    /// `work` says what the holder is doing, for the log line a waiting
    /// instance writes.
    pub async fn acquire(pool: &PgPool, key: i64, work: &str) -> anyhow::Result<Self> {
        let mut conn = pool.acquire().await?;
        conn.close_on_drop();
        let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await?;
        if !free {
            tracing::info!("Another instance is {work}; waiting for it to finish");
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(key)
                .execute(&mut *conn)
                .await?;
        }
        Ok(Self { conn })
    }

    /// The connection holding the lock. The work runs on it.
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.conn
    }

    /// Release the lock by ending the session.
    pub async fn release(self) {
        if let Err(e) = self.conn.close().await {
            // The session is gone either way, and the lock with it.
            tracing::debug!("closing the start-up lock's connection: {e}");
        }
    }
}

/// Bring the database up to the schema declared in `db/schema.sql`,
/// then idempotent-apply seeds from `db/seeds.sql`. Both files are
/// embedded at compile time, so the running binary never needs disk
/// access for them.
///
/// **Why not `sqlx::migrate!()`**: ThinkWatch's `db/schema.sql` is
/// declarative — every statement is `CREATE … IF NOT EXISTS` /
/// `OR REPLACE` / `DROP IF EXISTS … + CREATE`. Re-running it on an
/// already-correct DB is a no-op. That removes the "migration history
/// stack" from the repo entirely; the file IS the desired state.
///
/// What this can't do: column rename, type narrowing, DROP COLUMN,
/// data backfills. Those need an explicit one-off SQL kept in
/// `db/release_migrations/` and applied by hand — or, when an upgrade has
/// to carry them out by itself, a conversion that runs here in one
/// transaction and finds nothing left to do on the next boot (the guard
/// settings, [`crate::guard_policy::legacy`]).
///
/// **One instance at a time.** Several replicas start together — a
/// first install with more than one, a rolling upgrade, an autoscaler —
/// and each runs this. Run side by side, the statements deadlock (each
/// session holding a lock on `api_keys` or its trigger that another
/// needs) or collide creating the same object on an empty database, and
/// the instances that lose exit. So all of it runs under
/// [`SCHEMA_LOCK`], on the connection that holds it: the next instance
/// waits, then applies a schema that is already there, seeds rows that
/// already exist, and finds nothing left to convert.
pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    let mut lock =
        StartupLock::acquire(pool, SCHEMA_LOCK, "setting up the database schema").await?;
    let conn = lock.conn();
    let schema = include_str!("../../../db/schema.sql");
    // `Executor::execute` rather than `RawSql::execute`: with a borrowed
    // connection, the compiler cannot prove the latter's future `Send`,
    // and a caller that spawns this would not compile.
    conn.execute(sqlx::raw_sql(schema))
        .await
        .map_err(|e| anyhow::anyhow!("apply db/schema.sql: {e}"))?;
    let seeds = include_str!("../../../db/seeds.sql");
    conn.execute(sqlx::raw_sql(seeds))
        .await
        .map_err(|e| anyhow::anyhow!("apply db/seeds.sql: {e}"))?;
    crate::guard_policy::legacy::upgrade(conn)
        .await
        .map_err(|e| anyhow::anyhow!("convert the previous guard settings: {e}"))?;
    lock.release().await;
    tracing::info!("Database schema + seeds applied");
    Ok(())
}
