use agent_bastion_common::dynamic_config::DynamicConfig;
use sqlx::PgPool;
use std::sync::Arc;

/// Background task that cleans up old data based on retention policies.
/// Runs daily.
pub fn spawn_data_retention_task(db: PgPool, config: Arc<DynamicConfig>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(86400));
        loop {
            interval.tick().await;
            if let Err(e) = run_retention_cleanup(&db, &config).await {
                tracing::error!("Data retention cleanup failed: {e}");
            }
        }
    });
}

async fn run_retention_cleanup(db: &PgPool, config: &DynamicConfig) -> anyhow::Result<()> {
    // 1. Clean up usage records
    let usage_days = config.data_retention_days_usage().await;
    if usage_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(usage_days);
        let result = sqlx::query("DELETE FROM usage_records WHERE created_at < $1")
            .bind(cutoff)
            .execute(db)
            .await?;

        if result.rows_affected() > 0 {
            tracing::info!(
                "Purged {} usage records older than {} days",
                result.rows_affected(),
                usage_days
            );
        }
    }

    // 2. Audit logs are stored in ClickHouse only; retention is managed by
    //    ClickHouse TTL (see deploy/clickhouse/init.sql).

    // 3. Purge soft-deleted records older than 30 days
    let soft_delete_cutoff = chrono::Utc::now() - chrono::Duration::days(30);

    let users = sqlx::query("DELETE FROM users WHERE deleted_at IS NOT NULL AND deleted_at < $1")
        .bind(soft_delete_cutoff)
        .execute(db)
        .await?;

    let keys = sqlx::query("DELETE FROM api_keys WHERE deleted_at IS NOT NULL AND deleted_at < $1")
        .bind(soft_delete_cutoff)
        .execute(db)
        .await?;

    let providers =
        sqlx::query("DELETE FROM providers WHERE deleted_at IS NOT NULL AND deleted_at < $1")
            .bind(soft_delete_cutoff)
            .execute(db)
            .await?;

    let total_purged = users.rows_affected() + keys.rows_affected() + providers.rows_affected();
    if total_purged > 0 {
        tracing::info!("Purged {total_purged} soft-deleted records older than 30 days");
    }

    Ok(())
}
