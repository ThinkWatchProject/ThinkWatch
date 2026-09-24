//! Limits repository — row-by-id access to the two limit side tables,
//! `rate_limit_rules` and `budget_caps`, for the bulk endpoints.
//!
//! `table` is always one of those two names, fixed by the caller, never
//! user input. Errors are returned as `sqlx::Error`: the bulk endpoints
//! report each row's failure in their own words.

use sqlx::PgPool;
use uuid::Uuid;

/// The row's `(subject_kind, subject_id)`; `None` when there is no such
/// row.
pub async fn subject_of(
    pool: &PgPool,
    table: &'static str,
    id: Uuid,
) -> Result<Option<(String, Uuid)>, sqlx::Error> {
    let lookup_sql = format!("SELECT subject_kind, subject_id FROM {table} WHERE id = $1");
    sqlx::query_as(&lookup_sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Turn the row off. Returns the number of rows touched (0 or 1).
pub async fn disable(pool: &PgPool, table: &'static str, id: Uuid) -> Result<u64, sqlx::Error> {
    let sql = format!("UPDATE {table} SET enabled = FALSE, updated_at = now() WHERE id = $1");
    let result = sqlx::query(&sql).bind(id).execute(pool).await?;
    Ok(result.rows_affected())
}

/// Returns the number of rows deleted (0 or 1).
pub async fn delete(pool: &PgPool, table: &'static str, id: Uuid) -> Result<u64, sqlx::Error> {
    let sql = format!("DELETE FROM {table} WHERE id = $1");
    let result = sqlx::query(&sql).bind(id).execute(pool).await?;
    Ok(result.rows_affected())
}
