//! API key repository — the `api_keys` table. Keys are soft-deleted
//! (`deleted_at`); "live" means not deleted. Revoked keys stay in the
//! table, archived, until the retention sweep hard-deletes them.
//!
//! Key generation, permission checks and allow-list validation stay in
//! `handlers::api_keys`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use think_watch_common::errors::AppError;
use think_watch_common::models::ApiKey;
use uuid::Uuid;

/// The owner of a live key.
pub async fn owner_of_live(pool: &PgPool, id: Uuid) -> Result<Option<Uuid>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM api_keys WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Whether `user_id` holds an MCP credential labelled `account_label`
/// for the server — what a key's `mcp_account_overrides` may point at.
pub async fn mcp_credential_exists(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
) -> Result<bool, AppError> {
    let exists: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM mcp_user_credentials
              WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3",
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .fetch_optional(pool)
    .await?;
    Ok(exists.is_some())
}

/// The list's row filter: live keys, or the revoke-archived ones. The
/// archived view leaves out keys soft-deleted along with their user —
/// those were not revoked.
fn visibility_clause(archived: bool) -> &'static str {
    if archived {
        "deleted_at IS NOT NULL \
         AND (disabled_reason = 'revoked' OR disabled_reason LIKE 'force_revoked:%')"
    } else {
        "deleted_at IS NULL"
    }
}

/// How many keys the list shows, across every user.
pub async fn count_all(pool: &PgPool, archived: bool) -> Result<i64, AppError> {
    let visibility_clause = visibility_clause(archived);
    Ok(sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM api_keys WHERE {visibility_clause}"
    ))
    .fetch_one(pool)
    .await?)
}

/// One page of the list across every user, newest first.
pub async fn list_all_page(
    pool: &PgPool,
    archived: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ApiKey>, AppError> {
    let visibility_clause = visibility_clause(archived);
    Ok(sqlx::query_as::<_, ApiKey>(&format!(
        "SELECT * FROM api_keys WHERE {visibility_clause} \
             ORDER BY created_at DESC LIMIT $1 OFFSET $2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// How many of one user's keys the list shows.
pub async fn count_for_user(pool: &PgPool, archived: bool, user_id: Uuid) -> Result<i64, AppError> {
    let visibility_clause = visibility_clause(archived);
    Ok(sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM api_keys WHERE {visibility_clause} AND user_id = $1"
    ))
    .bind(user_id)
    .fetch_one(pool)
    .await?)
}

/// One page of one user's keys, newest first.
pub async fn list_for_user_page(
    pool: &PgPool,
    archived: bool,
    user_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ApiKey>, AppError> {
    let visibility_clause = visibility_clause(archived);
    Ok(sqlx::query_as::<_, ApiKey>(&format!(
        "SELECT * FROM api_keys WHERE {visibility_clause} AND user_id = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    ))
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// A key about to be created. `id` doubles as the lineage id: a new key
/// is the root of its own rotation chain.
pub struct NewApiKey<'a> {
    pub id: Uuid,
    pub key_prefix: &'a str,
    pub key_hash: &'a str,
    pub name: &'a str,
    pub user_id: Uuid,
    pub surfaces: &'a [String],
    pub allowed_models: &'a Option<Vec<String>>,
    pub allowed_mcp_tools: &'a Option<Vec<String>>,
    pub mcp_account_overrides: &'a serde_json::Value,
    pub expires_at: Option<DateTime<Utc>>,
    pub cost_center: Option<&'a str>,
    pub rotation_period_days: Option<i32>,
}

pub async fn insert(pool: &PgPool, key: &NewApiKey<'_>) -> Result<ApiKey, AppError> {
    Ok(sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (id, lineage_id, key_prefix, key_hash, name, user_id, surfaces,
                allowed_models, allowed_mcp_tools, mcp_account_overrides, expires_at,
                cost_center, rotation_period_days)
           VALUES ($1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING *"#,
    )
    .bind(key.id)
    .bind(key.key_prefix)
    .bind(key.key_hash)
    .bind(key.name)
    .bind(key.user_id)
    .bind(key.surfaces)
    .bind(key.allowed_models)
    .bind(key.allowed_mcp_tools)
    .bind(key.mcp_account_overrides)
    .bind(key.expires_at)
    .bind(key.cost_center)
    .bind(key.rotation_period_days)
    .fetch_one(pool)
    .await?)
}

pub async fn find_live(pool: &PgPool, id: Uuid) -> Result<Option<ApiKey>, AppError> {
    Ok(
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Revoke and archive a live key, ending any rotation grace window.
/// Returns how many rows changed (0 when the key is gone).
pub async fn revoke(pool: &PgPool, id: Uuid) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE api_keys SET is_active = false, grace_period_ends_at = NULL, \
                disabled_reason = 'revoked', deleted_at = now() \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Like [`revoke`], recording `disabled_reason` (a `force_revoked:` tag).
pub async fn force_revoke(pool: &PgPool, id: Uuid, disabled_reason: &str) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE api_keys SET is_active = false, grace_period_ends_at = NULL, \
                disabled_reason = $1, deleted_at = now() \
          WHERE id = $2 AND deleted_at IS NULL",
    )
    .bind(disabled_reason)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// A PATCH to a key's settings. Each `*_set` flag says whether its value
/// replaces the column (a `None` value then clears it); `surfaces`,
/// `rotation_period_days` and `inactivity_timeout_days` keep the column
/// when `None`. `expires_at` is always written.
pub struct ApiKeyPatch<'a> {
    pub allowed_models_set: bool,
    pub allowed_models: Option<&'a [String]>,
    pub allowed_mcp_tools_set: bool,
    pub allowed_mcp_tools: Option<&'a [String]>,
    pub surfaces: Option<&'a Vec<String>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub rotation_period_days: Option<i32>,
    pub inactivity_timeout_days: Option<i32>,
    pub cost_center_set: bool,
    pub cost_center: Option<&'a str>,
    pub mcp_account_overrides_set: bool,
    pub mcp_account_overrides: &'a serde_json::Value,
    /// Reset the expiry-warning dedupe, for an expiry pushed later.
    pub expiry_extended: bool,
}

pub async fn update(pool: &PgPool, id: Uuid, patch: &ApiKeyPatch<'_>) -> Result<ApiKey, AppError> {
    Ok(sqlx::query_as::<_, ApiKey>(
        r#"UPDATE api_keys SET
            allowed_models = CASE WHEN $11 THEN $1 ELSE allowed_models END,
            allowed_mcp_tools = CASE WHEN $12 THEN $10 ELSE allowed_mcp_tools END,
            surfaces = COALESCE($2, surfaces),
            expires_at = $3,
            rotation_period_days = COALESCE($4, rotation_period_days),
            inactivity_timeout_days = COALESCE($5, inactivity_timeout_days),
            cost_center = CASE WHEN $7 THEN $6 ELSE cost_center END,
            mcp_account_overrides = CASE WHEN $13 THEN $14 ELSE mcp_account_overrides END,
            last_expiry_warning_days = CASE WHEN $9 THEN NULL
                                            ELSE last_expiry_warning_days END
           WHERE id = $8 RETURNING *"#,
    )
    .bind(patch.allowed_models)
    .bind(patch.surfaces)
    .bind(patch.expires_at)
    .bind(patch.rotation_period_days)
    .bind(patch.inactivity_timeout_days)
    .bind(patch.cost_center)
    .bind(patch.cost_center_set)
    .bind(id)
    .bind(patch.expiry_extended)
    .bind(patch.allowed_mcp_tools)
    .bind(patch.allowed_models_set)
    .bind(patch.allowed_mcp_tools_set)
    .bind(patch.mcp_account_overrides_set)
    .bind(patch.mcp_account_overrides)
    .fetch_one(pool)
    .await?)
}

/// Rotate `old_key`: insert its successor (same name, owner, scope,
/// expiry and lineage; `rotated_from_id` pointing back) and put the old
/// key into its grace window, in one transaction — otherwise a failure
/// between the two would leave both keys valid with no grace end.
/// Returns the new key.
pub async fn rotate(
    pool: &PgPool,
    old_key: &ApiKey,
    key_prefix: &str,
    key_hash: &str,
    grace_period_ends_at: DateTime<Utc>,
) -> Result<ApiKey, AppError> {
    let mut tx = pool.begin().await?;

    let new_key = sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, surfaces, allowed_models,
            allowed_mcp_tools, expires_at, rotation_period_days, inactivity_timeout_days,
            cost_center, rotated_from_id, last_rotation_at, lineage_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), $13)
           RETURNING *"#,
    )
    .bind(key_prefix)
    .bind(key_hash)
    .bind(&old_key.name)
    .bind(old_key.user_id)
    .bind(&old_key.surfaces)
    .bind(&old_key.allowed_models)
    .bind(&old_key.allowed_mcp_tools)
    .bind(old_key.expires_at)
    .bind(old_key.rotation_period_days)
    .bind(old_key.inactivity_timeout_days)
    .bind(old_key.cost_center.as_deref())
    .bind(old_key.id)
    .bind(old_key.lineage_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE api_keys SET grace_period_ends_at = $1, disabled_reason = 'rotated' WHERE id = $2",
    )
    .bind(grace_period_ends_at)
    .bind(old_key.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_key)
}

/// Active live keys across every user that expire by `threshold`,
/// soonest first.
pub async fn list_expiring_all(
    pool: &PgPool,
    threshold: DateTime<Utc>,
) -> Result<Vec<ApiKey>, AppError> {
    Ok(sqlx::query_as::<_, ApiKey>(
        r#"SELECT * FROM api_keys
               WHERE is_active = true
                 AND deleted_at IS NULL
                 AND expires_at IS NOT NULL
                 AND expires_at <= $1
               ORDER BY expires_at ASC"#,
    )
    .bind(threshold)
    .fetch_all(pool)
    .await?)
}

/// [`list_expiring_all`], for one user's keys.
pub async fn list_expiring_for_user(
    pool: &PgPool,
    threshold: DateTime<Utc>,
    user_id: Uuid,
) -> Result<Vec<ApiKey>, AppError> {
    Ok(sqlx::query_as::<_, ApiKey>(
        r#"SELECT * FROM api_keys
               WHERE is_active = true
                 AND deleted_at IS NULL
                 AND expires_at IS NOT NULL
                 AND expires_at <= $1
                 AND user_id = $2
               ORDER BY expires_at ASC"#,
    )
    .bind(threshold)
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Distinct cost-center tags on live keys, alphabetical.
pub async fn cost_centers(pool: &PgPool) -> Result<Vec<String>, AppError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT cost_center FROM api_keys \
          WHERE cost_center IS NOT NULL AND deleted_at IS NULL \
          ORDER BY cost_center ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}
