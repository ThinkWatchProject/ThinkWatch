//! Log forwarder repository — the `log_forwarders` table: the
//! destinations audit and gateway logs are shipped to.

use sqlx::PgPool;
use think_watch_common::errors::AppError;
use think_watch_common::models::LogForwarder;
use uuid::Uuid;

/// Newest first, capped at 500.
pub async fn list(pool: &PgPool) -> Result<Vec<LogForwarder>, AppError> {
    Ok(sqlx::query_as::<_, LogForwarder>(
        "SELECT * FROM log_forwarders ORDER BY created_at DESC, id DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<LogForwarder>, AppError> {
    Ok(
        sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn create(
    pool: &PgPool,
    name: &str,
    forwarder_type: &str,
    config: &serde_json::Value,
    enabled: bool,
    log_types: &[String],
) -> Result<LogForwarder, AppError> {
    Ok(sqlx::query_as::<_, LogForwarder>(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, enabled, log_types)
           VALUES ($1, $2, $3, $4, $5) RETURNING *"#,
    )
    .bind(name)
    .bind(forwarder_type)
    .bind(config)
    .bind(enabled)
    .bind(log_types)
    .fetch_one(pool)
    .await?)
}

/// Overwrite the editable fields of an existing forwarder.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    config: &serde_json::Value,
    enabled: bool,
    log_types: &[String],
) -> Result<LogForwarder, AppError> {
    Ok(sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET name = $2, config = $3, enabled = $4, log_types = $5, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(config)
    .bind(enabled)
    .bind(log_types)
    .fetch_one(pool)
    .await?)
}

/// Returns the number of rows deleted (0 or 1).
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM log_forwarders WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// `None` when no such forwarder exists.
pub async fn set_enabled(
    pool: &PgPool,
    id: Uuid,
    enabled: bool,
) -> Result<Option<LogForwarder>, AppError> {
    Ok(sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET enabled = $2, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(enabled)
    .fetch_optional(pool)
    .await?)
}

/// Zero the sent / error counters and clear the last error. `None` when
/// no such forwarder exists.
pub async fn reset_stats(pool: &PgPool, id: Uuid) -> Result<Option<LogForwarder>, AppError> {
    Ok(sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET sent_count = 0, error_count = 0, last_error = NULL, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}
