//! Provider repository — the `providers` table. Rows are soft-deleted
//! (`deleted_at`); "live" means not deleted.
//!
//! Secrets in `config_json` are stored encrypted; encrypting on the way in
//! and redacting on the way out stay in `handlers::providers`.

use sqlx::PgPool;
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;
use uuid::Uuid;

/// Every live provider, newest first.
pub async fn list_live(pool: &PgPool) -> Result<Vec<Provider>, AppError> {
    Ok(sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE deleted_at IS NULL ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find_live(pool: &PgPool, id: Uuid) -> Result<Option<Provider>, AppError> {
    Ok(sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// A provider's name, deleted or not.
pub async fn name_of(pool: &PgPool, id: Uuid) -> Result<Option<String>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT name FROM providers WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn insert(
    pool: &PgPool,
    name: &str,
    display_name: &str,
    provider_type: &str,
    base_url: &str,
    config_json: &serde_json::Value,
) -> Result<Provider, AppError> {
    Ok(sqlx::query_as::<_, Provider>(
        r#"INSERT INTO providers (name, display_name, provider_type, base_url, config_json)
           VALUES ($1, $2, $3, $4, $5) RETURNING *"#,
    )
    .bind(name)
    .bind(display_name)
    .bind(provider_type)
    .bind(base_url)
    .bind(config_json)
    .fetch_one(pool)
    .await?)
}

pub async fn update(
    pool: &PgPool,
    id: Uuid,
    display_name: &str,
    base_url: &str,
    config_json: &serde_json::Value,
) -> Result<Provider, AppError> {
    Ok(sqlx::query_as::<_, Provider>(
        r#"UPDATE providers SET display_name = $2, base_url = $3, config_json = $4
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(base_url)
    .bind(config_json)
    .fetch_one(pool)
    .await?)
}

/// Forget the protocol learned for each of a provider's routes. Returns
/// how many routes had one.
pub async fn clear_learned_protocols(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE model_routes SET upstream_protocol = NULL
         WHERE provider_id = $1 AND upstream_protocol IS NOT NULL",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Soft-delete a provider and drop its routes, in one transaction. The
/// `model_routes` FK cascades on a real DELETE, not on flipping
/// `deleted_at`, so the routes are deleted explicitly — orphans would show
/// up on the Models page with a raw provider id and no way to edit them.
/// Returns how many routes went.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let routes_deleted = sqlx::query("DELETE FROM model_routes WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(routes_deleted)
}
