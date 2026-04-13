use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::CreateMcpServerRequest;
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ---------------------------------------------------------------------------
// Test MCP server connection — probe via JSON-RPC tools/list without persisting
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct TestMcpServerRequest {
    pub endpoint_url: String,
    pub auth_type: Option<String>,
    pub auth_secret: Option<String>,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TestMcpServerResponse {
    pub success: bool,
    pub message: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolSummary>>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct McpToolSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/mcp/servers/test",
    tag = "MCP Servers",
    security(("bearer_token" = [])),
    request_body = TestMcpServerRequest,
    responses(
        (status = 200, description = "Connection test result", body = TestMcpServerResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn test_mcp_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<TestMcpServerRequest>,
) -> Result<Json<TestMcpServerResponse>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    if req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("endpoint_url is required".into()));
    }
    super::providers::validate_url(&req.endpoint_url)?;
    if let Some(ref headers) = req.custom_headers {
        super::providers::validate_custom_headers(headers)?;
    }

    // Build the JSON-RPC tools/list request
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut builder = state
        .http_client
        .post(&req.endpoint_url)
        .header("Content-Type", "application/json")
        .json(&body);

    // Attach auth header
    match req.auth_type.as_deref() {
        Some("bearer") => {
            if let Some(ref secret) = req.auth_secret {
                builder = builder.header("Authorization", format!("Bearer {secret}"));
            }
        }
        Some("api_key") => {
            if let Some(ref secret) = req.auth_secret {
                builder = builder.header("X-API-Key", secret);
            }
        }
        _ => {}
    }

    // Attach custom headers
    if let Some(ref headers) = req.custom_headers {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
    }

    let started = std::time::Instant::now();
    let result = builder.send().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => {
            if !resp.status().is_success() {
                return Ok(Json(TestMcpServerResponse {
                    success: false,
                    message: format!("HTTP {}", resp.status()),
                    latency_ms,
                    tools_count: None,
                    tools: None,
                }));
            }
            let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let result_field = json.get("result");

            // Parse tools from result.tools
            let tools: Vec<McpToolSummary> = result_field
                .and_then(|r| r.get("tools"))
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            t.get("name")
                                .and_then(|n| n.as_str())
                                .map(|name| McpToolSummary {
                                    name: name.to_string(),
                                    description: t
                                        .get("description")
                                        .and_then(|d| d.as_str())
                                        .map(|s| s.to_string()),
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();

            if tools.is_empty() && result_field.is_none() {
                return Ok(Json(TestMcpServerResponse {
                    success: false,
                    message: "Invalid response: missing `result` field".into(),
                    latency_ms,
                    tools_count: None,
                    tools: None,
                }));
            }

            let count = tools.len();
            Ok(Json(TestMcpServerResponse {
                success: true,
                message: format!("Connected — {count} tools available"),
                latency_ms,
                tools_count: Some(count),
                tools: Some(tools),
            }))
        }
        Err(e) => Ok(Json(TestMcpServerResponse {
            success: false,
            message: format!("Request failed: {e}"),
            latency_ms,
            tools_count: None,
            tools: None,
        })),
    }
}

#[utoipa::path(
    get,
    path = "/api/mcp/servers",
    tag = "MCP Servers",
    responses(
        (status = 200, description = "List of all MCP servers"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_servers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServer>>, AppError> {
    auth_user.require_permission("mcp_servers:read")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:read")
        .await?;
    let servers = sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, COALESCE(t.cnt, 0) AS tools_count
           FROM mcp_servers s
           LEFT JOIN (SELECT server_id, COUNT(*) AS cnt FROM mcp_tools WHERE is_active = true GROUP BY server_id) t
             ON t.server_id = s.id
           ORDER BY s.created_at DESC"#,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(servers))
}

#[utoipa::path(
    post,
    path = "/api/mcp/servers",
    tag = "MCP Servers",
    request_body(content = inline(serde_json::Value), description = "CreateMcpServerRequest"),
    responses(
        (status = 200, description = "Newly created MCP server"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;
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

    // Auto-detect transport type if not explicitly specified
    let transport_type = if let Some(ref tt) = req.transport_type {
        tt.clone()
    } else {
        let auth_hdr =
            build_auth_probe_header(req.auth_type.as_deref(), req.auth_secret.as_deref());
        let auth_ref = auth_hdr.as_ref().map(|(n, v)| (n.as_str(), v.as_str()));
        match think_watch_mcp_gateway::detect::detect_transport(
            &state.http_client,
            &req.endpoint_url,
            auth_ref,
        )
        .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => "streamable_http".to_owned(),
        }
    };

    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (name, description, endpoint_url, transport_type, auth_type, auth_secret_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(&transport_type)
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

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub endpoint_url: Option<String>,
    pub transport_type: Option<String>,
    pub auth_type: Option<String>,
    pub auth_secret: Option<String>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    /// Values may contain `{{user_id}}` / `{{user_email}}` template variables.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[utoipa::path(
    patch,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    request_body(content = UpdateMcpServerRequest),
    responses(
        (status = 200, description = "Updated MCP server"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:update")
        .await?;
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
    let auth_type = req.auth_type.as_deref().or(existing.auth_type.as_deref());

    if req.endpoint_url.is_some() {
        super::providers::validate_url(endpoint_url)?;
    }

    // Auto-detect transport type when endpoint changes, otherwise keep existing
    let transport_type = if req.endpoint_url.is_some() {
        let auth_hdr = build_auth_probe_header(auth_type, req.auth_secret.as_deref());
        let auth_ref = auth_hdr.as_ref().map(|(n, v)| (n.as_str(), v.as_str()));
        match think_watch_mcp_gateway::detect::detect_transport(
            &state.http_client,
            endpoint_url,
            auth_ref,
        )
        .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => existing.transport_type.clone(),
        }
    } else if let Some(ref tt) = req.transport_type {
        tt.clone()
    } else {
        existing.transport_type.clone()
    };

    // Encrypt new auth secret if provided
    let auth_encrypted = if let Some(ref secret) = req.auth_secret {
        if secret.is_empty() {
            None
        } else {
            let key = crypto::parse_encryption_key(&state.config.encryption_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
            Some(
                crypto::encrypt(secret.as_bytes(), &key)
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?,
            )
        }
    } else {
        existing.auth_secret_encrypted.clone()
    };

    // Merge custom_headers + identity_headers into existing config_json
    let mut config_json = existing.config_json.clone();
    if let Some(ref headers) = req.custom_headers {
        super::providers::validate_custom_headers(headers)?;
        config_json["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
    }

    let updated = sqlx::query_as::<_, McpServer>(
        r#"UPDATE mcp_servers SET name = $2, description = $3, endpoint_url = $4,
           transport_type = $5, auth_type = $6, auth_secret_encrypted = $7, config_json = $8
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(endpoint_url)
    .bind(transport_type)
    .bind(auth_type)
    .bind(&auth_encrypted)
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

#[utoipa::path(
    get,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    responses(
        (status = 200, description = "MCP server details"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:read")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:read")
        .await?;
    let server = sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    Ok(Json(server))
}

#[utoipa::path(
    delete,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    responses(
        (status = 200, description = "Server deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:delete")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:delete")
        .await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;

    // Decrement install_count if this server was installed from the store
    sqlx::query(
        r#"UPDATE mcp_store_templates SET install_count = GREATEST(install_count - 1, 0)
           WHERE id = (SELECT template_id FROM mcp_store_installs WHERE server_id = $1)"#,
    )
    .bind(id)
    .execute(&state.db)
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

/// Build an `(header_name, header_value)` pair for transport-detection probes.
pub fn build_auth_probe_header(
    auth_type: Option<&str>,
    auth_secret: Option<&str>,
) -> Option<(String, String)> {
    let secret = auth_secret.filter(|s| !s.is_empty())?;
    match auth_type? {
        "bearer" => Some(("Authorization".to_owned(), format!("Bearer {secret}"))),
        "api_key" => Some(("X-API-Key".to_owned(), secret.to_owned())),
        _ => None,
    }
}
