use sqlx::PgPool;
use std::sync::Arc;
use think_watch_common::dynamic_config::DynamicConfig;

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
    //    ClickHouse TTL (see deploy/clickhouse/initdb.d/01_init.sql).

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

    // 4. Orphan cleanup for the polymorphic limits engine.
    //
    // `rate_limit_rules` and `budget_caps` use a polymorphic
    // `(subject_kind, subject_id)` pair instead of a real foreign
    // key, so when the subject row is hard-deleted (step 3 above)
    // its rules are left dangling forever. The list endpoints
    // happily return the orphans, the limits engine evaluates them
    // against UUIDs that no longer exist, and they accumulate
    // across rotations.
    //
    // We can't add a real FK because the column is multi-typed
    // (user / api_key / provider / mcp_server), so we sweep here
    // instead. The query is one DELETE per kind with a NOT EXISTS
    // anti-join — runs in milliseconds even at high cardinality
    // because both sides are indexed on UUID.
    let orphan_rules = sqlx::query(
        r#"DELETE FROM rate_limit_rules
           WHERE (subject_kind = 'user'        AND NOT EXISTS (SELECT 1 FROM users        u WHERE u.id  = subject_id))
              OR (subject_kind = 'api_key'    AND NOT EXISTS (SELECT 1 FROM api_keys     k WHERE k.id  = subject_id))
              OR (subject_kind = 'provider'   AND NOT EXISTS (SELECT 1 FROM providers    p WHERE p.id  = subject_id))
              OR (subject_kind = 'mcp_server' AND NOT EXISTS (SELECT 1 FROM mcp_servers  s WHERE s.id  = subject_id))"#,
    )
    .execute(db)
    .await?;

    let orphan_caps = sqlx::query(
        r#"DELETE FROM budget_caps
           WHERE (subject_kind = 'user'      AND NOT EXISTS (SELECT 1 FROM users     u WHERE u.id  = subject_id))
              OR (subject_kind = 'api_key'  AND NOT EXISTS (SELECT 1 FROM api_keys  k WHERE k.id  = subject_id))
              OR (subject_kind = 'team'     AND NOT EXISTS (SELECT 1 FROM teams     t WHERE t.id  = subject_id))
              OR (subject_kind = 'provider' AND NOT EXISTS (SELECT 1 FROM providers p WHERE p.id  = subject_id))"#,
    )
    .execute(db)
    .await?;

    let orphan_total = orphan_rules.rows_affected() + orphan_caps.rows_affected();
    if orphan_total > 0 {
        tracing::info!(
            "Purged {orphan_total} orphaned limits rows ({} rate-limit rules, {} budget caps)",
            orphan_rules.rows_affected(),
            orphan_caps.rows_affected()
        );
    }

    Ok(())
}
