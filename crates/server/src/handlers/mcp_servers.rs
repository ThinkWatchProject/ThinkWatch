use axum::extract::{Path, State};
use axum::Json;
use uuid::Uuid;

use agent_bastion_common::crypto;
use agent_bastion_common::dto::CreateMcpServerRequest;
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

pub async fn list_servers(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServer>>, AppError> {
    let servers = sqlx::query_as::<_, McpServer>(
        "SELECT * FROM mcp_servers ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(servers))
}

pub async fn create_server(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    if req.name.is_empty() || req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("name and endpoint_url are required".into()));
    }

    // SSRF prevention: validate endpoint_url
    super::providers::validate_url(&req.endpoint_url)?;

    let auth_encrypted = if let Some(ref secret) = req.auth_secret {
        let key = crypto::parse_encryption_key(&state.config.encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        Some(crypto::encrypt(secret.as_bytes(), &key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?)
    } else {
        None
    };

    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (name, description, endpoint_url, transport_type, auth_type, auth_secret_encrypted)
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(req.transport_type.as_deref().unwrap_or("streamable_http"))
    .bind(&req.auth_type)
    .bind(&auth_encrypted)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(server))
}

pub async fn get_server(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServer>, AppError> {
    let server = sqlx::query_as::<_, McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    Ok(Json(server))
}

pub async fn delete_server(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    Ok(Json(serde_json::json!({"status": "deleted"})))
}
