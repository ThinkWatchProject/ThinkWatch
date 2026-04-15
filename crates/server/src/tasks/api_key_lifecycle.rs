use sqlx::PgPool;
use std::sync::Arc;
use think_watch_common::audit::{AuditEntry, AuditLogger};
use think_watch_common::dynamic_config::DynamicConfig;

/// Background task that manages API key lifecycle:
/// - Disables expired keys
/// - Disables inactive keys
/// - Revokes rotated keys past their grace period
/// - Emits `key.expiry_warning` audit events when a key crosses one of
///   the 7 / 3 / 1 day-remaining thresholds.
///
/// Runs every hour.
pub fn spawn_api_key_lifecycle_task(db: PgPool, config: Arc<DynamicConfig>, audit: AuditLogger) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = run_lifecycle_check(&db, &config, &audit).await {
                tracing::error!("API key lifecycle check failed: {e}");
            }
        }
    });
}

async fn run_lifecycle_check(
    db: &PgPool,
    config: &DynamicConfig,
    audit: &AuditLogger,
) -> anyhow::Result<()> {
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

    // 4. Emit `key.expiry_warning` events as keys cross 7 / 3 / 1 day
    //    thresholds. `last_expiry_warning_days` pins the lowest bucket
    //    we've already warned about; this query picks up every key
    //    whose current remaining-days bucket is lower (or was never
    //    warned) AND whose key is still active / not expired / not
    //    already in rotation grace.
    //
    //    Runs as one atomic UPDATE ... RETURNING so each bucket
    //    transition fires exactly once across hourly runs, even if
    //    two replicas of the task happen to tick simultaneously.
    #[derive(sqlx::FromRow)]
    struct WarningRow {
        id: uuid::Uuid,
        bucket: i32,
        expires_at: chrono::DateTime<chrono::Utc>,
        user_id: Option<uuid::Uuid>,
    }
    let warnings: Vec<WarningRow> = sqlx::query_as(
        r#"WITH candidates AS (
             SELECT id,
                    expires_at,
                    user_id,
                    CASE
                        WHEN expires_at <= now() + interval '1 day' THEN 1
                        WHEN expires_at <= now() + interval '3 days' THEN 3
                        WHEN expires_at <= now() + interval '7 days' THEN 7
                        ELSE NULL
                    END AS bucket
             FROM api_keys
             WHERE is_active = true
               AND deleted_at IS NULL
               AND expires_at IS NOT NULL
               AND expires_at > now()
               AND grace_period_ends_at IS NULL
           )
           UPDATE api_keys k
              SET last_expiry_warning_days = c.bucket
             FROM candidates c
            WHERE k.id = c.id
              AND c.bucket IS NOT NULL
              AND (k.last_expiry_warning_days IS NULL
                   OR c.bucket < k.last_expiry_warning_days)
           RETURNING k.id, c.bucket::int AS "bucket!: i32",
                     k.expires_at AS "expires_at!: chrono::DateTime<chrono::Utc>",
                     k.user_id"#,
    )
    .fetch_all(db)
    .await?;

    for w in warnings {
        let mut entry = AuditEntry::new("key.expiry_warning")
            .resource(format!("api_key:{}", w.id))
            .detail(serde_json::json!({
                "api_key_id": w.id.to_string(),
                "expires_at": w.expires_at.to_rfc3339(),
                "days_remaining_bucket": w.bucket,
            }));
        if let Some(uid) = w.user_id {
            entry = entry.user_id(uid);
        }
        audit.log(entry);
    }

    Ok(())
}
