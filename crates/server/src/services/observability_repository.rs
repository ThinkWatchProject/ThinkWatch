//! Observability repository — the Postgres reads behind the dashboard
//! tiles, the live snapshot, the health probes and the per-model route
//! health view, plus the per-user dashboard layout.
//!
//! Functions whose callers map database errors themselves (a custom
//! message, or a best-effort fallback) return `sqlx::Error` unchanged.

use sqlx::PgPool;
use think_watch_common::errors::AppError;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Health probes
// ---------------------------------------------------------------------------

/// `SELECT 1` — is Postgres answering?
pub async fn ping(pool: &PgPool) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
}

/// Active, non-deleted providers — the readiness check and the
/// dashboard tile.
pub async fn count_active_providers(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------------------
// Dashboard scope
// ---------------------------------------------------------------------------

/// Does the user hold `analytics:read_all` through a global role?
pub async fn has_global_analytics_read_all(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM rbac_role_assignments ra
               JOIN rbac_roles r ON r.id = ra.role_id
              WHERE ra.user_id = $1
                AND ra.scope_kind = 'global'
                AND EXISTS (
                    SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                    WHERE stmt->>'Effect' = 'Allow'
                      AND (stmt->>'Action' = '*' OR stmt->>'Action' = 'analytics:read_all'
                           OR (stmt->'Action' @> '\"analytics:read_all\"'::jsonb))
                )
         )",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Ids (as text) of the user plus every live member of any team the
/// user holds `analytics:read_team` or `analytics:read_all` for at team
/// scope.
pub async fn analytics_team_scope_user_ids(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<(String,)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT DISTINCT u.id::text
           FROM users u
          WHERE u.deleted_at IS NULL
            AND (u.id = $1
             OR EXISTS (
                 SELECT 1 FROM team_members tm
                   JOIN rbac_role_assignments ra ON ra.scope_kind = 'team'
                                                 AND ra.scope_id = tm.team_id
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE tm.user_id = u.id
                    AND ra.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND (stmt->>'Action' = '*'
                               OR stmt->>'Action' = 'analytics:read_team'
                               OR stmt->>'Action' = 'analytics:read_all'
                               OR (stmt->'Action' @> '\"analytics:read_team\"'::jsonb)
                               OR (stmt->'Action' @> '\"analytics:read_all\"'::jsonb))
                    )
             ))",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Dashboard stats
// ---------------------------------------------------------------------------

/// Ids (as text) of the caller plus every live member of the given
/// teams.
pub async fn caller_and_team_member_ids(
    pool: &PgPool,
    caller_id: Uuid,
    team_ids: &[Uuid],
) -> Result<Vec<(String,)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT u.id::text FROM users u \
         WHERE u.deleted_at IS NULL AND (u.id = $1 \
            OR EXISTS ( \
                SELECT 1 FROM team_members tm \
                 WHERE tm.user_id = u.id AND tm.team_id = ANY($2) \
            ))",
    )
    .bind(caller_id)
    .bind(team_ids)
    .fetch_all(pool)
    .await?)
}

/// MCP servers whose status is `connected`.
pub async fn count_connected_mcp_servers(pool: &PgPool) -> Result<Option<i64>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE status = 'connected'")
            .fetch_one(pool)
            .await?,
    )
}

/// Active keys used since `since` — the fallback when ClickHouse is off.
pub async fn count_api_keys_used_since(
    pool: &PgPool,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(DISTINCT id) FROM api_keys \
         WHERE is_active = true AND deleted_at IS NULL \
           AND last_used_at >= $1",
    )
    .bind(since)
    .fetch_one(pool)
    .await?)
}

/// Active keys last used in `[start, end)`.
pub async fn count_api_keys_used_between(
    pool: &PgPool,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>, AppError> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(DISTINCT id) FROM api_keys \
         WHERE is_active = true AND deleted_at IS NULL \
           AND last_used_at >= $1 AND last_used_at < $2",
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?)
}

// ---------------------------------------------------------------------------
// Dashboard live snapshot
// ---------------------------------------------------------------------------

/// Every route of an active provider, with that provider's name.
pub async fn active_provider_routes(pool: &PgPool) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT mr.id, p.name FROM model_routes mr \
           JOIN providers p ON p.id = mr.provider_id \
          WHERE p.is_active = true AND p.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await
}

/// Names of the active, non-deleted providers.
pub async fn active_provider_names(pool: &PgPool) -> Result<Vec<(String,)>, sqlx::Error> {
    sqlx::query_as::<_, (String,)>(
        "SELECT name FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await
}

/// Every MCP server's name and status.
pub async fn mcp_server_statuses(pool: &PgPool) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String)>("SELECT name, status FROM mcp_servers")
        .fetch_all(pool)
        .await
}

/// Highest per-minute request limit across the enabled rules.
pub async fn max_enabled_rpm_limit(pool: &PgPool) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(max_count) FROM rate_limit_rules \
         WHERE metric = 'requests' AND window_secs = 60 AND enabled = true",
    )
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------------------
// Dashboard layout
// ---------------------------------------------------------------------------

/// The user's saved layout: `(name, layout_json)`.
pub async fn get_layout(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<(String, serde_json::Value)>, AppError> {
    Ok(
        sqlx::query_as("SELECT name, layout_json FROM user_dashboard_layouts WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Insert or replace the user's layout.
pub async fn upsert_layout(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    layout_json: &serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO user_dashboard_layouts (user_id, name, layout_json, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (user_id) DO UPDATE \
           SET name = EXCLUDED.name, \
               layout_json = EXCLUDED.layout_json, \
               updated_at = now()",
    )
    .bind(user_id)
    .bind(name)
    .bind(layout_json)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Route health
// ---------------------------------------------------------------------------

/// One route of a model, as the route-health view lists it.
#[derive(sqlx::FromRow)]
pub struct ModelRouteRow {
    pub route_id: Uuid,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: String,
    pub weight: i32,
    pub enabled: bool,
}

/// The model's routes on non-deleted providers, heaviest first.
pub async fn model_routes(pool: &PgPool, model_id: &str) -> Result<Vec<ModelRouteRow>, AppError> {
    Ok(sqlx::query_as(
        r#"SELECT mr.id AS route_id, mr.provider_id,
                  p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id
            WHERE mr.model_id = $1 AND p.deleted_at IS NULL
            ORDER BY mr.weight DESC"#,
    )
    .bind(model_id)
    .fetch_all(pool)
    .await?)
}
