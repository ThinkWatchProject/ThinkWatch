use sqlx::PgPool;
use std::sync::Arc;
use think_watch_common::audit::{AuditEntry, AuditLogger};
use think_watch_common::dynamic_config::DynamicConfig;

// DynamicConfig is still threaded through from the call site but
// `run_retention_cleanup` no longer reads any values off it — every
// remaining cleanup uses SOFT_DELETE_RETENTION_DAYS as a const.
// Kept in the signature so adding a new config-driven retention knob
// (e.g. per-user soft-delete override) stays a one-line change.

/// Retention window for soft-deleted rows before they are hard-deleted.
/// 30 days matches the GDPR "right to be forgotten" outer bound while
/// leaving operators a practical restoration window for accidental
/// deletes. Changing the value is a policy decision — update the
/// retention docs (deploy/README) and bump the audit entry below
/// together so the paper trail stays coherent.
const SOFT_DELETE_RETENTION_DAYS: i64 = 30;

/// Background task that cleans up old data based on retention policies.
/// Runs daily.
pub fn spawn_data_retention_task(db: PgPool, config: Arc<DynamicConfig>, audit: AuditLogger) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(86400));
        loop {
            interval.tick().await;
            if let Err(e) = run_retention_cleanup(&db, &config, &audit).await {
                tracing::error!("Data retention cleanup failed: {e}");
            }
        }
    });
}

/// Single deterministic retention pass. `pub` so integration tests
/// can drive it without sleeping for 24h.
pub async fn run_retention_cleanup(
    db: &PgPool,
    _config: &DynamicConfig,
    audit: &AuditLogger,
) -> anyhow::Result<()> {
    // 1. Usage rows live in ClickHouse `gateway_logs` only; retention
    //    is enforced by the table's `TTL toDateTime(created_at)` clause
    //    (see deploy/clickhouse/initdb.d/01_init.sql). No Postgres
    //    purge is needed — the legacy `usage_records` table was
    //    dropped when analytics moved to CH.
    //
    // 2. Audit logs follow the same story — CH TTL, no PG purge.

    // 3. Purge soft-deleted records older than the GDPR retention
    // window. This is the irreversible hard-delete — we emit an audit
    // entry per table with a non-zero row count so the compliance trail
    // shows when and how many records left the system.
    let soft_delete_cutoff =
        chrono::Utc::now() - chrono::Duration::days(SOFT_DELETE_RETENTION_DAYS);

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
        tracing::info!(
            "Purged {total_purged} soft-deleted records older than {} days",
            SOFT_DELETE_RETENTION_DAYS
        );
        audit.log(
            AuditEntry::new("data.gdpr_purge")
                .resource("data_retention")
                .detail(serde_json::json!({
                    "retention_days": SOFT_DELETE_RETENTION_DAYS,
                    "cutoff": soft_delete_cutoff.to_rfc3339(),
                    "users_purged": users.rows_affected(),
                    "api_keys_purged": keys.rows_affected(),
                    "providers_purged": providers.rows_affected(),
                })),
        );
    }

    // 4. Webhook outbox garbage collection. Rows that have exhausted
    // their retry budget accumulate forever otherwise and bloat the
    // admin outbox page. Seven days gives operators a window to see
    // recent delivery failures, then the failed entry is dropped.
    let outbox_cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let outbox_purged = sqlx::query(
        "DELETE FROM webhook_outbox \
          WHERE attempts >= 10 \
            AND next_attempt_at < $1",
    )
    .bind(outbox_cutoff)
    .execute(db)
    .await?;
    if outbox_purged.rows_affected() > 0 {
        tracing::info!(
            "Purged {} exhausted webhook_outbox rows older than 7 days",
            outbox_purged.rows_affected()
        );
    }

    // 5. Orphan cleanup for the polymorphic limits engine.
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
