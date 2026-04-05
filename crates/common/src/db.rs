use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub async fn create_pool(database_url: &str) -> anyhow::Result<PgPool> {
    if !database_url.contains("sslmode=") {
        tracing::warn!("DATABASE_URL does not specify sslmode. Use sslmode=require in production.");
    }

    let max_connections: u32 = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);

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

pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations").run(pool).await?;
    tracing::info!("Database migrations applied");
    Ok(())
}
