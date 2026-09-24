//! MCP store repository — `mcp_store_templates` (the catalog synced from
//! a remote registry) and `mcp_store_installs` (which server came from
//! which template).
//!
//! Registry fetching, template validation and audit stay in
//! `handlers::mcp_store`; installing runs inside the server-create
//! transaction in `handlers::mcp_servers`.

use sqlx::{PgConnection, PgPool};
use think_watch_common::errors::AppError;
use think_watch_common::models::McpStoreTemplate;
use uuid::Uuid;

/// Process-wide advisory-lock key for serializing template installs.
/// The literal spells "mcpStore" in ASCII so a DBA glancing at
/// `pg_locks` can tell what's holding it. Any new advisory lock
/// added elsewhere in the codebase MUST use a distinct constant —
/// collisions silently serialize unrelated work and can deadlock
/// under concurrent load.
///
/// Reserved advisory lock keys (keep this list current):
///   * `MCP_STORE_INSTALL_LOCK_KEY` (here): template-install
///     serialization in `create_server` when `template_slug` is set.
const MCP_STORE_INSTALL_LOCK_KEY: i64 = 0x6D637053746F7265;

/// A category and how many templates are in it.
#[derive(sqlx::FromRow)]
pub struct CategoryCountRow {
    pub category: Option<String>,
    pub count: Option<i64>,
}

/// One registry template as written to `mcp_store_templates`.
pub struct TemplateUpsert<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub category: Option<&'a str>,
    pub tags: &'a [String],
    pub endpoint_template: Option<&'a str>,
    pub oauth_issuer: Option<&'a str>,
    pub oauth_authorization_endpoint: Option<&'a str>,
    pub oauth_token_endpoint: Option<&'a str>,
    pub oauth_revocation_endpoint: Option<&'a str>,
    pub oauth_userinfo_endpoint: Option<&'a str>,
    pub oauth_default_scopes: &'a [String],
    pub auth_shape: &'a str,
    pub static_token_help_url: Option<&'a str>,
    pub auth_header_name: &'a str,
    pub auth_value_template: &'a str,
    pub auth_instructions: Option<&'a str>,
    pub deploy_type: &'a str,
    pub deploy_command: Option<&'a str>,
    pub deploy_docs_url: Option<&'a str>,
    pub homepage_url: Option<&'a str>,
    pub repo_url: Option<&'a str>,
    pub featured: bool,
}

/// Every template that has been installed at least once.
pub async fn installed_template_ids(pool: &PgPool) -> Result<Vec<Uuid>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT template_id FROM mcp_store_installs")
            .fetch_all(pool)
            .await?,
    )
}

/// Every template, featured and most-installed first.
pub async fn list_templates(pool: &PgPool) -> Result<Vec<McpStoreTemplate>, AppError> {
    Ok(sqlx::query_as::<_, McpStoreTemplate>(
        "SELECT * FROM mcp_store_templates ORDER BY featured DESC, install_count DESC, name ASC",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find_template_by_slug(
    pool: &PgPool,
    slug: &str,
) -> Result<Option<McpStoreTemplate>, AppError> {
    Ok(
        sqlx::query_as::<_, McpStoreTemplate>("SELECT * FROM mcp_store_templates WHERE slug = $1")
            .bind(slug)
            .fetch_optional(pool)
            .await?,
    )
}

/// Template counts per category, largest first.
pub async fn category_counts(pool: &PgPool) -> Result<Vec<CategoryCountRow>, AppError> {
    Ok(sqlx::query_as::<_, CategoryCountRow>(
        "SELECT category, COUNT(*) as count FROM mcp_store_templates GROUP BY category ORDER BY count DESC",
    )
    .fetch_all(pool)
    .await?)
}

/// Insert or refresh one template by slug, inside the caller's sync
/// transaction.
pub async fn upsert_template(
    conn: &mut PgConnection,
    t: &TemplateUpsert<'_>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_store_templates
               (slug, name, description, category, tags, endpoint_template,
                oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
                oauth_revocation_endpoint, oauth_userinfo_endpoint,
                oauth_default_scopes,
                auth_shape, static_token_help_url,
                auth_header_name, auth_value_template,
                auth_instructions, deploy_type,
                deploy_command, deploy_docs_url, homepage_url, repo_url, featured, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                       $16, $17, $18, $19, $20, $21, $22, $23, now())
               ON CONFLICT (slug) DO UPDATE SET
                 name = EXCLUDED.name,
                 description = EXCLUDED.description,
                 category = EXCLUDED.category,
                 tags = EXCLUDED.tags,
                 endpoint_template = EXCLUDED.endpoint_template,
                 oauth_issuer = EXCLUDED.oauth_issuer,
                 oauth_authorization_endpoint = EXCLUDED.oauth_authorization_endpoint,
                 oauth_token_endpoint = EXCLUDED.oauth_token_endpoint,
                 oauth_revocation_endpoint = EXCLUDED.oauth_revocation_endpoint,
                 oauth_userinfo_endpoint = EXCLUDED.oauth_userinfo_endpoint,
                 oauth_default_scopes = EXCLUDED.oauth_default_scopes,
                 auth_shape = EXCLUDED.auth_shape,
                 static_token_help_url = EXCLUDED.static_token_help_url,
                 auth_header_name = EXCLUDED.auth_header_name,
                 auth_value_template = EXCLUDED.auth_value_template,
                 auth_instructions = EXCLUDED.auth_instructions,
                 deploy_type = EXCLUDED.deploy_type,
                 deploy_command = EXCLUDED.deploy_command,
                 deploy_docs_url = EXCLUDED.deploy_docs_url,
                 homepage_url = EXCLUDED.homepage_url,
                 repo_url = EXCLUDED.repo_url,
                 featured = EXCLUDED.featured,
                 updated_at = now()"#,
    )
    .bind(t.slug)
    .bind(t.name)
    .bind(t.description)
    .bind(t.category)
    .bind(t.tags)
    .bind(t.endpoint_template)
    .bind(t.oauth_issuer)
    .bind(t.oauth_authorization_endpoint)
    .bind(t.oauth_token_endpoint)
    .bind(t.oauth_revocation_endpoint)
    .bind(t.oauth_userinfo_endpoint)
    .bind(t.oauth_default_scopes)
    .bind(t.auth_shape)
    .bind(t.static_token_help_url)
    .bind(t.auth_header_name)
    .bind(t.auth_value_template)
    .bind(t.auth_instructions)
    .bind(t.deploy_type)
    .bind(t.deploy_command)
    .bind(t.deploy_docs_url)
    .bind(t.homepage_url)
    .bind(t.repo_url)
    .bind(t.featured)
    .execute(conn)
    .await?;
    Ok(())
}

/// Delete the templates whose slug isn't in `keep_slugs`, except those
/// with installs, inside the caller's sync transaction. Returns how many
/// went.
pub async fn delete_templates_not_in(
    conn: &mut PgConnection,
    keep_slugs: &[&str],
) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"WITH deleted AS (
             DELETE FROM mcp_store_templates
             WHERE slug != ALL($1)
               AND id NOT IN (SELECT template_id FROM mcp_store_installs)
             RETURNING 1
           )
           SELECT COUNT(*) FROM deleted"#,
    )
    .bind(keep_slugs)
    .fetch_one(conn)
    .await?)
}

/// Serialize template installs for the rest of the caller's transaction,
/// so two concurrent installs can't resolve to the same server name.
pub async fn lock_installs(conn: &mut PgConnection) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MCP_STORE_INSTALL_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

/// A template's id by slug, row-locked for the caller's transaction.
pub async fn lock_template_by_slug(
    conn: &mut PgConnection,
    slug: &str,
) -> Result<Option<Uuid>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM mcp_store_templates WHERE slug = $1 FOR UPDATE")
            .bind(slug)
            .fetch_optional(conn)
            .await?,
    )
}

/// Record that a server was installed from a template and bump the
/// template's `install_count`, inside the caller's transaction.
pub async fn record_install(
    conn: &mut PgConnection,
    template_id: Uuid,
    server_id: Uuid,
    installed_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO mcp_store_installs (template_id, server_id, installed_by) VALUES ($1, $2, $3)",
    )
    .bind(template_id)
    .bind(server_id)
    .bind(installed_by)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE mcp_store_templates SET install_count = install_count + 1 WHERE id = $1")
        .bind(template_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
