use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::CreateMcpServerRequest;
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

pub async fn list_servers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServer>>, AppError> {
    auth_user.require_permission("mcp_servers:read")?;
    let servers =
        sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers ORDER BY created_at DESC")
            .fetch_all(&state.db)
            .await?;

    Ok(Json(servers))
}

pub async fn create_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    if req.name.is_empty() || req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest(
            "name and endpoint_url are required".into(),
        ));
    }

    // SSRF prevention: validate endpoint_url
    super::providers::validate_url(&req.endpoint_url)?;

    let auth_encrypted = if let Some(ref secret) = req.auth_secret {
        let key = crypto::parse_encryption_key(&state.config.encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        Some(
            crypto::encrypt(secret.as_bytes(), &key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?,
        )
    } else {
        None
    };

    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (name, description, endpoint_url, transport_type, auth_type, auth_secret_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(req.transport_type.as_deref().unwrap_or("streamable_http"))
    .bind(&req.auth_type)
    .bind(&auth_encrypted)
    .bind({
        let mut config = serde_json::json!({});
        if let Some(ref headers) = req.custom_headers {
            super::providers::validate_custom_headers(headers)?;
            config["custom_headers"] = serde_json::to_value(headers).unwrap_or_default();
        }
        config
    })
    .fetch_one(&state.db)
    .await?;

    // Sync the in-memory MCP registry so the gateway can route to the new
    // server immediately, without a restart. The CB is also pre-registered
    // so the dashboard upstream-health panel reflects it on next snapshot.
    if let Ok(registered) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &server,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(registered).await;
        state.mcp_circuit_breakers.register(&server.name).await;
    }

    // Kick off tool discovery in the background — adding a server in the
    // UI should not block on a slow upstream tools/list, but the metadata
    // should arrive shortly after so the admin sees its tools.
    {
        let db = state.db.clone();
        let key = state.config.encryption_key.clone();
        let http = state.http_client.clone();
        let registry = state.mcp_registry.clone();
        let server = server.clone();
        // R4.2: capture the latest discovery error onto mcp_servers.last_error
        // so the admin UI can surface it without needing log access. The
        // task is fire-and-forget but its outcome is now persisted.
        let server_id = server.id;
        let db_for_err = state.db.clone();
        tokio::spawn(async move {
            match crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server, &key).await {
                Ok(n) => {
                    tracing::info!(
                        mcp_server = %server.name,
                        tools = n,
                        "MCP tool discovery completed for new server"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                    if let Ok(updated) =
                        crate::mcp_runtime::build_registered_server(&db, &server, &key).await
                    {
                        registry.register(updated).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "Initial MCP tool discovery failed"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
                        .bind(format!("{e}"))
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                }
            }
        });
    }

    state.audit.log(
        auth_user
            .audit("mcp_server.created")
            .resource("mcp_server")
            .resource_id(server.id.to_string())
            .detail(serde_json::json!({ "name": &req.name })),
    );

    Ok(Json(server))
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub endpoint_url: Option<String>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

pub async fn update_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    let existing = sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    let name = req.name.as_deref().unwrap_or(&existing.name);
    let description = req
        .description
        .as_deref()
        .or(existing.description.as_deref());
    let endpoint_url = req
        .endpoint_url
        .as_deref()
        .unwrap_or(&existing.endpoint_url);

    if req.endpoint_url.is_some() {
        super::providers::validate_url(endpoint_url)?;
    }

    // Merge custom_headers into existing config_json
    let config_json = if let Some(ref headers) = req.custom_headers {
        super::providers::validate_custom_headers(headers)?;
        let mut config = existing.config_json.clone();
        config["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
        config
    } else {
        existing.config_json.clone()
    };

    let updated = sqlx::query_as::<_, McpServer>(
        r#"UPDATE mcp_servers SET name = $2, description = $3, endpoint_url = $4, config_json = $5
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(endpoint_url)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await?;

    // Evict any cached connection first — the pool keys by id, so a
    // changed endpoint URL would otherwise keep using the old one.
    state.mcp_pool.remove(id).await;

    // Re-register so the in-memory registry picks up the new endpoint /
    // name. `register` is an upsert keyed by id.
    if let Ok(registered) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &updated,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(registered).await;
        state.mcp_circuit_breakers.register(&updated.name).await;
    }

    state.audit.log(
        auth_user
            .audit("mcp_server.updated")
            .resource("mcp_server")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.name })),
    );

    Ok(Json(updated))
}

pub async fn get_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:read")?;
    let server = sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    Ok(Json(server))
}

pub async fn delete_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:delete")?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;

    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    // Drop from the in-memory registry and connection pool — otherwise the
    // gateway would keep a stale entry for a server that no longer exists
    // in the database.
    state.mcp_registry.unregister(id).await;
    state.mcp_pool.remove(id).await;

    state.audit.log(
        auth_user
            .audit("mcp_server.deleted")
            .resource("mcp_server")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}
