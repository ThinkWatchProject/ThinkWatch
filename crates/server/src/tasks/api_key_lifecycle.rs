use sqlx::PgPool;
use std::sync::Arc;
use think_watch_common::audit::{AuditActor, AuditLogger, SystemActor};
use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::tasks::supervise_restart;

/// Background task that manages API key lifecycle:
/// - Disables expired keys
/// - Disables inactive keys
/// - Revokes rotated keys past their grace period
/// - Emits `key.expiry_warning` audit events when a key crosses one of
///   the 7 / 3 / 1 day-remaining thresholds.
///
/// Runs every 10 minutes. The auth middleware also applies the
/// inactivity and grace-period cutoffs at request time (lazy
/// disable), so this loop is a bookkeeping backstop — it keeps
/// `is_active` / `disabled_reason` in sync and fires the expiry-
/// warning audit events on the 7 / 3 / 1-day thresholds.
pub fn spawn_api_key_lifecycle_task(db: PgPool, config: Arc<DynamicConfig>, audit: AuditLogger) {
    // Wrapped in supervise_restart: a panic inside this loop would
    // silently stop disabling expired/inactive keys and stop firing
    // expiry-warning audit events — a real compliance gap that
    // operators wouldn't notice until a customer reported being
    // unable to log in (or worse, AFTER an expired key was used).
    supervise_restart("api_key_lifecycle", move || {
        let db = db.clone();
        let config = config.clone();
        let audit = audit.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                interval.tick().await;
                if let Err(e) = run_lifecycle_check(&db, &config, &audit).await {
                    tracing::error!("API key lifecycle check failed: {e}");
                }
            }
        }
    });
}

/// One row's worth of "this key just changed state" — fed into the
/// audit-emission loop so every disable/revoke leaves a compliance
/// trail, not just a `tracing::info!` line.
#[derive(sqlx::FromRow)]
struct DisabledKeyRow {
    id: uuid::Uuid,
    user_id: Option<uuid::Uuid>,
}

fn emit_disable_audits(audit: &AuditLogger, action: &str, rows: &[DisabledKeyRow], reason: &str) {
    for row in rows {
        let mut entry = SystemActor
            .audit(action)
            .resource(format!("api_key:{}", row.id))
            .detail(serde_json::json!({
                "api_key_id": row.id.to_string(),
                "disabled_reason": reason,
            }));
        if let Some(uid) = row.user_id {
            entry = entry.user_id(uid);
        }
        audit.log(entry);
    }
}

/// Single deterministic pass of the lifecycle loop. `pub` so
/// integration tests can drive it without `tokio::time::pause`-ing
/// the whole 10-minute interval.
pub async fn run_lifecycle_check(
    db: &PgPool,
    config: &DynamicConfig,
    audit: &AuditLogger,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    // 1. Disable expired keys. Previously this query used `.execute()`
    //    and `tracing::info!`'d the count; the actual state changes
    //    landed in PG with NO audit trail, so security teams investigating
    //    "why did this key stop working" had to cross-reference application
    //    logs against the DB and hope they hadn't rotated. RETURNING +
    //    audit emit closes the gap. Same change applied to all four
    //    disable/revoke queries below.
    let expired: Vec<DisabledKeyRow> = sqlx::query_as(
        r#"UPDATE api_keys
           SET is_active = false, disabled_reason = 'expired'
           WHERE is_active = true
             AND expires_at IS NOT NULL
             AND expires_at < $1
             AND disabled_reason IS NULL
           RETURNING id, user_id"#,
    )
    .bind(now)
    .fetch_all(db)
    .await?;

    if !expired.is_empty() {
        tracing::info!("Disabled {} expired API keys", expired.len());
        emit_disable_audits(audit, "api_key.disabled", &expired, "expired");
    }

    // 2. Disable inactive keys
    let global_inactivity_days = config.api_keys_inactivity_timeout_days().await;
    if global_inactivity_days > 0 {
        let inactive_threshold = now - chrono::Duration::days(global_inactivity_days);
        let inactive: Vec<DisabledKeyRow> = sqlx::query_as(
            r#"UPDATE api_keys
               SET is_active = false, disabled_reason = 'inactive'
               WHERE is_active = true
                 AND last_used_at IS NOT NULL
                 AND last_used_at < $1
                 AND disabled_reason IS NULL
                 AND (inactivity_timeout_days IS NULL OR inactivity_timeout_days = 0)
               RETURNING id, user_id"#,
        )
        .bind(inactive_threshold)
        .fetch_all(db)
        .await?;

        if !inactive.is_empty() {
            tracing::info!(
                "Disabled {} inactive API keys (global timeout: {} days)",
                inactive.len(),
                global_inactivity_days
            );
            emit_disable_audits(audit, "api_key.disabled", &inactive, "inactive_global");
        }
    }

    // Per-key inactivity timeout
    let per_key_inactive: Vec<DisabledKeyRow> = sqlx::query_as(
        r#"UPDATE api_keys
           SET is_active = false, disabled_reason = 'inactive'
           WHERE is_active = true
             AND inactivity_timeout_days IS NOT NULL
             AND inactivity_timeout_days > 0
             AND last_used_at IS NOT NULL
             AND last_used_at < now() - (inactivity_timeout_days || ' days')::interval
             AND disabled_reason IS NULL
           RETURNING id, user_id"#,
    )
    .fetch_all(db)
    .await?;

    if !per_key_inactive.is_empty() {
        tracing::info!(
            "Disabled {} inactive API keys (per-key timeout)",
            per_key_inactive.len()
        );
        emit_disable_audits(
            audit,
            "api_key.disabled",
            &per_key_inactive,
            "inactive_per_key",
        );
    }

    // 3. Revoke rotated keys past grace period
    let grace_expired: Vec<DisabledKeyRow> = sqlx::query_as(
        r#"UPDATE api_keys
           SET is_active = false
           WHERE is_active = true
             AND grace_period_ends_at IS NOT NULL
             AND grace_period_ends_at < $1
           RETURNING id, user_id"#,
    )
    .bind(now)
    .fetch_all(db)
    .await?;

    if !grace_expired.is_empty() {
        tracing::info!(
            "Revoked {} rotated API keys past grace period",
            grace_expired.len()
        );
        emit_disable_audits(
            audit,
            "api_key.revoked",
            &grace_expired,
            "grace_period_expired",
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
        // System task — no human actor. `user_id` is the *subject* of
        // the warning (whose key is expiring), not the actor, so it's
        // chained on after `SystemActor.audit(...)`.
        let mut entry = SystemActor
            .audit("key.expiry_warning")
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
