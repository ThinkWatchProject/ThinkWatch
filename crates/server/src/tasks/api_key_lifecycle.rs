use sqlx::PgPool;
use std::sync::Arc;
use think_watch_common::dynamic_config::DynamicConfig;

/// Background task that manages API key lifecycle:
/// - Disables expired keys
/// - Disables inactive keys
/// - Revokes rotated keys past their grace period
///
/// Runs every hour.
pub fn spawn_api_key_lifecycle_task(db: PgPool, config: Arc<DynamicConfig>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = run_lifecycle_check(&db, &config).await {
                tracing::error!("API key lifecycle check failed: {e}");
            }
        }
    });
}

async fn run_lifecycle_check(db: &PgPool, config: &DynamicConfig) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    // 1. Disable expired keys
    let expired = sqlx::query(
        r#"UPDATE api_keys
           SET is_active = false, disabled_reason = 'expired'
           WHERE is_active = true
             AND expires_at IS NOT NULL
             AND expires_at < $1
             AND disabled_reason IS NULL"#,
    )
    .bind(now)
    .execute(db)
    .await?;

    if expired.rows_affected() > 0 {
        tracing::info!("Disabled {} expired API keys", expired.rows_affected());
    }

    // 2. Disable inactive keys
    let global_inactivity_days = config.api_keys_inactivity_timeout_days().await;
    if global_inactivity_days > 0 {
        let inactive_threshold = now - chrono::Duration::days(global_inactivity_days);
        let inactive = sqlx::query(
            r#"UPDATE api_keys
               SET is_active = false, disabled_reason = 'inactive'
               WHERE is_active = true
                 AND last_used_at IS NOT NULL
                 AND last_used_at < $1
                 AND disabled_reason IS NULL
                 AND (inactivity_timeout_days IS NULL OR inactivity_timeout_days = 0)"#,
        )
        .bind(inactive_threshold)
        .execute(db)
        .await?;

        if inactive.rows_affected() > 0 {
            tracing::info!(
                "Disabled {} inactive API keys (global timeout: {} days)",
                inactive.rows_affected(),
                global_inactivity_days
            );
        }
    }

    // Per-key inactivity timeout
    let per_key_inactive = sqlx::query(
        r#"UPDATE api_keys
           SET is_active = false, disabled_reason = 'inactive'
           WHERE is_active = true
             AND inactivity_timeout_days IS NOT NULL
             AND inactivity_timeout_days > 0
             AND last_used_at IS NOT NULL
             AND last_used_at < now() - (inactivity_timeout_days || ' days')::interval
             AND disabled_reason IS NULL"#,
    )
    .execute(db)
    .await?;

    if per_key_inactive.rows_affected() > 0 {
        tracing::info!(
            "Disabled {} inactive API keys (per-key timeout)",
            per_key_inactive.rows_affected()
        );
    }

    // 3. Revoke rotated keys past grace period
    let grace_expired = sqlx::query(
        r#"UPDATE api_keys
           SET is_active = false
           WHERE is_active = true
             AND grace_period_ends_at IS NOT NULL
             AND grace_period_ends_at < $1"#,
    )
    .bind(now)
    .execute(db)
    .await?;

    if grace_expired.rows_affected() > 0 {
        tracing::info!(
            "Revoked {} rotated API keys past grace period",
            grace_expired.rows_affected()
        );
    }

    Ok(())
}
