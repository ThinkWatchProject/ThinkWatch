//! Webhook outbox repository — the admin view of `webhook_outbox`:
//! webhook deliveries waiting for (another) attempt.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use think_watch_common::errors::AppError;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct WebhookOutboxRow {
    pub id: Uuid,
    pub forwarder_id: Uuid,
    /// Looked up at list time so the UI can render a name without a
    /// second round-trip. `None` means the forwarder was deleted —
    /// the FK CASCADE should normally clean those up but a row could
    /// linger if the worker is mid-iteration.
    pub forwarder_name: Option<String>,
    /// URL the delivery is targeting, extracted from the forwarder
    /// config. Lets the operator debug a stuck row without jumping
    /// to the forwarder-admin page to cross-reference. `None` when
    /// the forwarder was deleted or the config is somehow missing
    /// the `url` field (defensive).
    pub forwarder_url: Option<String>,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Pending rows, next due first, capped at 200; every forwarder's when
/// `forwarder_id` is `None`.
pub async fn list(
    pool: &PgPool,
    forwarder_id: Option<Uuid>,
) -> Result<Vec<WebhookOutboxRow>, AppError> {
    // `$1::uuid IS NULL OR o.forwarder_id = $1` lets one prepared
    // statement serve both the "show everything" and "only this
    // forwarder" calls. `->>` returns TEXT for the URL column —
    // safer than a second materialised column that'd drift from the
    // forwarder's canonical config.
    Ok(sqlx::query_as(
        "SELECT o.id, o.forwarder_id, f.name AS forwarder_name, \
                (f.config->>'url')::text AS forwarder_url, \
                o.attempts, o.next_attempt_at, o.last_error, o.created_at \
           FROM webhook_outbox o \
           LEFT JOIN log_forwarders f ON f.id = o.forwarder_id \
          WHERE $1::uuid IS NULL OR o.forwarder_id = $1 \
          ORDER BY o.next_attempt_at ASC \
          LIMIT 200",
    )
    .bind(forwarder_id)
    .fetch_all(pool)
    .await?)
}

/// Pending rows in total; every forwarder's when `forwarder_id` is `None`.
pub async fn count(pool: &PgPool, forwarder_id: Option<Uuid>) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_outbox \
          WHERE $1::uuid IS NULL OR forwarder_id = $1",
    )
    .bind(forwarder_id)
    .fetch_one(pool)
    .await?)
}

/// `(forwarder_id, pending rows)`, biggest backlog first, capped at 500.
pub async fn counts_by_forwarder(pool: &PgPool) -> Result<Vec<(Uuid, i64)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT forwarder_id, COUNT(*) AS count \
           FROM webhook_outbox \
          GROUP BY forwarder_id \
          ORDER BY count DESC \
          LIMIT 500",
    )
    .fetch_all(pool)
    .await?)
}

/// Returns the number of rows deleted (0 or 1).
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Make the row due now. Returns the number of rows touched (0 or 1).
pub async fn retry_now(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    let result = sqlx::query("UPDATE webhook_outbox SET next_attempt_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
