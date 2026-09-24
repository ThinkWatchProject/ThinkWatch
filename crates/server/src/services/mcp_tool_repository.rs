//! MCP tool catalog repository — `mcp_tools` (discovered per server)
//! unioned with a user's own `mcp_user_tools`, as listed on
//! `/api/mcp/tools`.
//!
//! The per-user catalog is pre-namespaced the same way mcp_tools is
//! (`<prefix>__<tool>`) and unioned with it. `mcp_user_tools` doesn't
//! carry an `id` column — synthesize a stable v5-style UUID from
//! `(server_id, user_id, tool_name)` so the frontend's keying
//! (`tool.id`) keeps working without a schema change.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use think_watch_common::errors::AppError;
use uuid::Uuid;

#[derive(Debug, Serialize, FromRow, utoipa::ToSchema)]
pub struct McpToolRow {
    #[schema(value_type = String, format = Uuid)]
    pub id: uuid::Uuid,
    #[schema(value_type = String, format = Uuid)]
    pub server_id: uuid::Uuid,
    pub server_name: String,
    pub name: String,
    pub namespaced_name: String,
    pub description: Option<String>,
    #[schema(value_type = Object)]
    pub input_schema: Option<serde_json::Value>,
}

/// Filter and page for [`count_catalog`] / [`list_catalog`].
pub struct CatalogQuery<'a> {
    /// Trimmed search text; empty matches everything.
    pub search: &'a str,
    /// `%search%`, matched with ILIKE.
    pub search_pattern: &'a str,
    pub server_id: Option<Uuid>,
    pub page_size: i64,
    pub offset: i64,
    /// Whose `mcp_user_tools` to include; `None` includes none.
    pub user_id: Option<Uuid>,
}

pub async fn count_catalog(pool: &PgPool, q: &CatalogQuery<'_>) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        r#"WITH catalog AS (
              SELECT t.id,
                     t.server_id,
                     s.name AS server_name,
                     s.namespace_prefix,
                     t.tool_name,
                     t.description
                FROM mcp_tools t
                JOIN mcp_servers s ON s.id = t.server_id
                WHERE t.is_active = true
              UNION ALL
              SELECT gen_random_uuid() AS id,
                     u.mcp_server_id AS server_id,
                     s.name AS server_name,
                     s.namespace_prefix,
                     u.tool_name,
                     u.description
                FROM mcp_user_tools u
                JOIN mcp_servers s ON s.id = u.mcp_server_id
                WHERE $6::uuid IS NOT NULL AND u.user_id = $6::uuid
            )
           SELECT COUNT(*) FROM catalog
            WHERE ($3::uuid IS NULL OR server_id = $3)
              AND ($1 = ''
                   OR tool_name ILIKE $2
                   OR (namespace_prefix || '__' || tool_name) ILIKE $2
                   OR COALESCE(description, '') ILIKE $2)"#,
    )
    .bind(q.search)
    .bind(q.search_pattern)
    .bind(q.server_id)
    .bind(q.page_size)
    .bind(q.offset)
    .bind(q.user_id)
    .fetch_one(pool)
    .await?)
}

/// One page of the catalog, by server name then tool name.
pub async fn list_catalog(
    pool: &PgPool,
    q: &CatalogQuery<'_>,
) -> Result<Vec<McpToolRow>, AppError> {
    Ok(sqlx::query_as::<_, McpToolRow>(
        r#"WITH catalog AS (
              SELECT t.id,
                     t.server_id,
                     s.name AS server_name,
                     s.namespace_prefix,
                     t.tool_name,
                     t.description,
                     t.input_schema
                FROM mcp_tools t
                JOIN mcp_servers s ON s.id = t.server_id
                WHERE t.is_active = true
              UNION ALL
              SELECT gen_random_uuid() AS id,
                     u.mcp_server_id AS server_id,
                     s.name AS server_name,
                     s.namespace_prefix,
                     u.tool_name,
                     u.description,
                     u.input_schema
                FROM mcp_user_tools u
                JOIN mcp_servers s ON s.id = u.mcp_server_id
                WHERE $6::uuid IS NOT NULL AND u.user_id = $6::uuid
            )
           SELECT id,
                  server_id,
                  server_name,
                  tool_name AS name,
                  namespace_prefix || '__' || tool_name AS namespaced_name,
                  description,
                  input_schema
             FROM catalog
            WHERE ($3::uuid IS NULL OR server_id = $3)
              AND ($1 = ''
                   OR tool_name ILIKE $2
                   OR (namespace_prefix || '__' || tool_name) ILIKE $2
                   OR COALESCE(description, '') ILIKE $2)
            ORDER BY server_name, tool_name
            LIMIT $4 OFFSET $5"#,
    )
    .bind(q.search)
    .bind(q.search_pattern)
    .bind(q.server_id)
    .bind(q.page_size)
    .bind(q.offset)
    .bind(q.user_id)
    .fetch_all(pool)
    .await?)
}
