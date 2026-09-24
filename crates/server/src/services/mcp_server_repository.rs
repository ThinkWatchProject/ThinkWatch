//! MCP server repository — the `mcp_servers` table, plus the credential
//! and store-count rows that change in the same transaction as a server
//! update or delete.
//!
//! Thin wrappers over sqlx, one statement (or one transaction) per
//! function; validation, secret encryption, registry sync and audit stay
//! in `handlers::mcp_servers`.

use sqlx::{PgConnection, PgPool};
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;
use uuid::Uuid;

/// The columns an admin writes when creating or updating a server.
pub struct McpServerFields<'a> {
    pub name: &'a str,
    pub namespace_prefix: &'a str,
    pub display_label: Option<&'a str>,
    pub description: Option<&'a str>,
    pub endpoint_url: &'a str,
    pub transport_type: &'a str,
    pub oauth_issuer: Option<&'a str>,
    pub oauth_authorization_endpoint: Option<&'a str>,
    pub oauth_token_endpoint: Option<&'a str>,
    pub oauth_revocation_endpoint: Option<&'a str>,
    pub oauth_userinfo_endpoint: Option<&'a str>,
    pub oauth_client_id: Option<&'a str>,
    pub oauth_client_secret_encrypted: Option<&'a [u8]>,
    pub oauth_scopes: &'a [String],
    pub auth_shape: &'a str,
    pub static_token_help_url: Option<&'a str>,
    pub auth_header_name: &'a str,
    pub auth_value_template: &'a str,
    pub credential_owner: &'a str,
    pub config_json: &'a serde_json::Value,
}

/// Every server with its active-tool count, newest first.
pub async fn list_with_tool_counts(pool: &PgPool) -> Result<Vec<McpServer>, AppError> {
    Ok(sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, COALESCE(t.cnt, 0) AS tools_count
           FROM mcp_servers s
           LEFT JOIN (SELECT server_id, COUNT(*) AS cnt FROM mcp_tools WHERE is_active = true GROUP BY server_id) t
             ON t.server_id = s.id
           ORDER BY s.created_at DESC"#,
    )
    .fetch_all(pool)
    .await?)
}

/// Every server by name, with zeroed tool and call counts.
pub async fn list_by_name(pool: &PgPool) -> Result<Vec<McpServer>, AppError> {
    Ok(sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, 0::bigint AS tools_count, 0::bigint AS call_count
             FROM mcp_servers s
            ORDER BY s.name"#,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<McpServer>, AppError> {
    Ok(
        sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// One server with zeroed tool and call counts.
pub async fn find_without_counts(pool: &PgPool, id: Uuid) -> Result<Option<McpServer>, AppError> {
    Ok(sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, 0::bigint AS tools_count, 0::bigint AS call_count
             FROM mcp_servers s WHERE s.id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Whether another server already uses this name or namespace prefix.
pub async fn name_or_prefix_taken(
    conn: &mut PgConnection,
    name: &str,
    namespace_prefix: &str,
) -> Result<bool, AppError> {
    // `SELECT 1` is INT4 on the wire; binding into `Option<i64>`
    // panics with a column-decode mismatch the moment a row
    // comes back. We don't actually care about the value — only
    // whether the row exists — so use Option<i32>.
    let conflict: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM mcp_servers WHERE name = $1 OR namespace_prefix = $2 LIMIT 1",
    )
    .bind(name)
    .bind(namespace_prefix)
    .fetch_optional(conn)
    .await?;
    Ok(conflict.is_some())
}

/// Insert a server inside the caller's transaction. A taken name or
/// prefix comes back as a 409.
pub async fn insert(
    conn: &mut PgConnection,
    f: &McpServerFields<'_>,
) -> Result<McpServer, AppError> {
    sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (
               name, namespace_prefix, display_label, description, endpoint_url, transport_type,
               oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
               oauth_revocation_endpoint, oauth_userinfo_endpoint,
               oauth_client_id, oauth_client_secret_encrypted,
               oauth_scopes, auth_shape, static_token_help_url,
               auth_header_name, auth_value_template, credential_owner,
               config_json
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                   $16, $17, $18, $19, $20)
           RETURNING *"#,
    )
    .bind(f.name)
    .bind(f.namespace_prefix)
    .bind(f.display_label)
    .bind(f.description)
    .bind(f.endpoint_url)
    .bind(f.transport_type)
    .bind(f.oauth_issuer)
    .bind(f.oauth_authorization_endpoint)
    .bind(f.oauth_token_endpoint)
    .bind(f.oauth_revocation_endpoint)
    .bind(f.oauth_userinfo_endpoint)
    .bind(f.oauth_client_id)
    .bind(f.oauth_client_secret_encrypted)
    .bind(f.oauth_scopes)
    .bind(f.auth_shape)
    .bind(f.static_token_help_url)
    .bind(f.auth_header_name)
    .bind(f.auth_value_template)
    .bind(f.credential_owner)
    .bind(f.config_json)
    .fetch_one(conn)
    .await
    .map_err(map_unique_violation)
}

/// Update a server and, in the same transaction, drop the credentials
/// the change made stale: per-user credentials and tool caches when
/// `purge_user_credentials`, the shared credential when
/// `purge_shared_credential`. A taken name or prefix comes back as a 409.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    f: &McpServerFields<'_>,
    purge_user_credentials: bool,
    purge_shared_credential: bool,
) -> Result<McpServer, AppError> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, McpServer>(
        r#"UPDATE mcp_servers SET
              name = $2, namespace_prefix = $3, display_label = $4,
              description = $5, endpoint_url = $6,
              transport_type = $7,
              oauth_issuer = $8, oauth_authorization_endpoint = $9,
              oauth_token_endpoint = $10, oauth_revocation_endpoint = $11,
              oauth_userinfo_endpoint = $12,
              oauth_client_id = $13, oauth_client_secret_encrypted = $14,
              oauth_scopes = $15, auth_shape = $16, static_token_help_url = $17,
              auth_header_name = $18, auth_value_template = $19, credential_owner = $20,
              config_json = $21
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(f.name)
    .bind(f.namespace_prefix)
    .bind(f.display_label)
    .bind(f.description)
    .bind(f.endpoint_url)
    .bind(f.transport_type)
    .bind(f.oauth_issuer)
    .bind(f.oauth_authorization_endpoint)
    .bind(f.oauth_token_endpoint)
    .bind(f.oauth_revocation_endpoint)
    .bind(f.oauth_userinfo_endpoint)
    .bind(f.oauth_client_id)
    .bind(f.oauth_client_secret_encrypted)
    .bind(f.oauth_scopes)
    .bind(f.auth_shape)
    .bind(f.static_token_help_url)
    .bind(f.auth_header_name)
    .bind(f.auth_value_template)
    .bind(f.credential_owner)
    .bind(f.config_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_unique_violation)?;

    if purge_user_credentials {
        sqlx::query("DELETE FROM mcp_user_credentials WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_user_tools WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if purge_shared_credential {
        sqlx::query("DELETE FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// Delete one server. Returns its name, or `None` (and changes nothing)
/// when there is no such server.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<Option<String>, AppError> {
    let mut tx = pool.begin().await?;
    let Some(name) = delete_in_tx(&mut tx, id).await? else {
        return Ok(None);
    };
    tx.commit().await?;
    Ok(Some(name))
}

/// Delete several servers in one transaction: all of them or none.
/// Returns each id with its name, or `None` where there was no such
/// server.
pub async fn delete_many(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Option<String>)>, AppError> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        out.push((id, delete_in_tx(&mut tx, id).await?));
    }
    tx.commit().await?;
    Ok(out)
}

/// Tear down a single MCP server inside the caller's transaction:
///   * SELECT the server name (returned to the caller for audit detail)
///   * decrement the originating store template's `install_count`
///   * DELETE the server row (children CASCADE: `mcp_tools`,
///     `mcp_user_credentials`, `mcp_server_shared_credentials`,
///     `mcp_user_tools`, `mcp_store_installs`)
///
/// Returns `Ok(Some(name))` on success, `Ok(None)` if the row doesn't
/// exist.
async fn delete_in_tx(conn: &mut PgConnection, id: Uuid) -> Result<Option<String>, AppError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    if name.is_none() {
        return Ok(None);
    }

    // Decrement install_count if this server was installed from the store.
    sqlx::query(
        r#"UPDATE mcp_store_templates SET install_count = GREATEST(install_count - 1, 0)
           WHERE id = (SELECT template_id FROM mcp_store_installs WHERE server_id = $1)"#,
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;

    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;

    Ok(name)
}

pub async fn clear_last_error(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_error(pool: &PgPool, id: Uuid, error: &str) -> Result<(), AppError> {
    sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Translate PostgreSQL unique-constraint violations on `mcp_servers` into
/// user-facing conflict errors, so the UI shows "already in use" instead of
/// a generic 500. Other sqlx errors fall through unchanged.
fn map_unique_violation(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db_err) = &e
        && db_err.code().as_deref() == Some("23505")
    {
        let constraint = db_err.constraint().unwrap_or("");
        if constraint.contains("namespace_prefix") {
            return AppError::Conflict("namespace_prefix already in use".into());
        }
        if constraint.contains("name") {
            return AppError::Conflict("server name already in use".into());
        }
        return AppError::Conflict("duplicate server".into());
    }
    AppError::from(e)
}
