use axum::Json;
use axum::extract::State;
use serde::Deserialize;

use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::McpTool;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

pub async fn list_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpTool>>, AppError> {
    let tools = sqlx::query_as::<_, McpTool>(
        "SELECT * FROM mcp_tools WHERE is_active = true ORDER BY server_id, tool_name",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(tools))
}

#[derive(Debug, Deserialize)]
struct McpToolDef {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct McpToolsListResult {
    tools: Vec<McpToolDef>,
}

pub async fn discover_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(server_id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let server = sqlx::query_as::<_, agent_bastion_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    // Connect to the MCP server and call tools/list via JSON-RPC
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client error: {e}")))?;

    let jsonrpc_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut req = client
        .post(&server.endpoint_url)
        .header("Content-Type", "application/json")
        .json(&jsonrpc_request);

    // Add auth header if configured
    if let Some(ref auth_type) = server.auth_type
        && let Some(ref encrypted) = server.auth_secret_encrypted
        && let Ok(key) =
            agent_bastion_common::crypto::parse_encryption_key(&state.config.encryption_key)
        && let Ok(secret) = agent_bastion_common::crypto::decrypt(encrypted, &key)
    {
        let secret_str = String::from_utf8_lossy(&secret);
        match auth_type.as_str() {
            "bearer" => {
                req = req.header("Authorization", format!("Bearer {secret_str}"));
            }
            "api_key" => {
                req = req.header("X-API-Key", secret_str.as_ref());
            }
            _ => {}
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to connect to MCP server: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "MCP server returned HTTP {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to parse MCP server response: {e}")))?;

    // Parse tools from JSON-RPC result
    let result = body.get("result").unwrap_or(&body);
    let tools_result: McpToolsListResult = serde_json::from_value(result.clone())
        .map_err(|e| AppError::BadRequest(format!("Invalid tools/list response: {e}")))?;

    // Deactivate all existing tools for this server, then upsert discovered ones
    sqlx::query("UPDATE mcp_tools SET is_active = false WHERE server_id = $1")
        .bind(server_id)
        .execute(&state.db)
        .await?;

    let mut count = 0usize;
    for tool in &tools_result.tools {
        sqlx::query(
            r#"INSERT INTO mcp_tools (server_id, tool_name, description, input_schema, is_active, discovered_at)
               VALUES ($1, $2, $3, $4, true, now())
               ON CONFLICT (server_id, tool_name)
               DO UPDATE SET description = $3, input_schema = $4, is_active = true, discovered_at = now()"#,
        )
        .bind(server_id)
        .bind(&tool.name)
        .bind(&tool.description)
        .bind(&tool.input_schema)
        .execute(&state.db)
        .await?;
        count += 1;
    }

    // Update server status
    sqlx::query(
        "UPDATE mcp_servers SET status = 'connected', last_health_check = now() WHERE id = $1",
    )
    .bind(server_id)
    .execute(&state.db)
    .await?;

    Ok(Json(serde_json::json!({
        "status": "discovery_complete",
        "server_id": server_id,
        "tools_discovered": count,
    })))
}
