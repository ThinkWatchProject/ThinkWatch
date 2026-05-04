use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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
/// `db/release_migrations/` and applied by hand.
pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    let schema = include_str!("../../../db/schema.sql");
    sqlx::raw_sql(schema)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("apply db/schema.sql: {e}"))?;
    let seeds = include_str!("../../../db/seeds.sql");
    sqlx::raw_sql(seeds)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("apply db/seeds.sql: {e}"))?;
    tracing::info!("Database schema + seeds applied");
    Ok(())
}
