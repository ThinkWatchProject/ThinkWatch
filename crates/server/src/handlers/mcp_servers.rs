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
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServer>>, AppError> {
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

    state.audit.log(
        auth_user
            .audit("mcp_server.updated")
            .resource("mcp_server")
            .resource_id(id.to_string()),
    );

    Ok(Json(updated))
}

pub async fn get_server(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServer>, AppError> {
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
    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    state.audit.log(
        auth_user
            .audit("mcp_server.deleted")
            .resource("mcp_server")
            .resource_id(id.to_string()),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}
