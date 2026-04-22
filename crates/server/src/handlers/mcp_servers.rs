use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::CreateMcpServerRequest;
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use super::serde_util::deserialize_some;
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
    /// If provided and `auth_secret` is empty, fall back to the stored
    /// encrypted secret from this server (used when editing a server
    /// without re-entering the credential).
    pub server_id: Option<Uuid>,
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

// `probe_mcp_endpoint`, `McpProbeOutcome`, `McpToolSummary`,
// `normalize_namespace_prefix`, and `build_auth_probe_header` moved
// to `super::mcp_shared` so mcp_store no longer reaches across
// handlers. Re-export them so existing in-module callers still work;
// new callers should import from `super::mcp_shared` directly.
pub use super::mcp_shared::{
    McpToolSummary, build_auth_probe_header, normalize_namespace_prefix, probe_mcp_endpoint,
};

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
    // The test endpoint makes arbitrary outbound HTTP requests, so
    // cap per-user calls at 5/min to prevent abuse as a port scanner.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp",
    )
    .await?;

    if req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("endpoint_url is required".into()));
    }
    think_watch_common::validation::validate_url(&req.endpoint_url)?;
    if let Some(ref headers) = req.custom_headers {
        think_watch_common::validation::validate_custom_headers(headers)?;
    }

    // Resolve the auth secret: prefer the one in the request, fall back to
    // the server's stored encrypted secret when `server_id` is supplied.
    let resolved_secret: Option<String> = if let Some(ref s) = req.auth_secret
        && !s.is_empty()
    {
        Some(s.clone())
    } else if let Some(sid) = req.server_id {
        let stored: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT auth_secret_encrypted FROM mcp_servers WHERE id = $1")
                .bind(sid)
                .fetch_optional(&state.db)
                .await?
                .flatten();
        match stored {
            Some(bytes) => {
                let key =
                    crypto::parse_encryption_key(&state.config.encryption_key).map_err(|e| {
                        AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}"))
                    })?;
                crypto::decrypt(&bytes, &key)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
            }
            None => None,
        }
    } else {
        None
    };

    let http = state.http_client.load();
    let outcome = probe_mcp_endpoint(
        &http,
        &req.endpoint_url,
        req.auth_type.as_deref(),
        resolved_secret.as_deref(),
        req.custom_headers.as_ref(),
    )
    .await;

    let tools_count = if outcome.success {
        Some(outcome.tools.len())
    } else {
        None
    };
    let tools = if outcome.success {
        Some(outcome.tools)
    } else {
        None
    };
    Ok(Json(TestMcpServerResponse {
        success: outcome.success,
        message: outcome.message,
        latency_ms: outcome.latency_ms,
        tools_count,
        tools,
    }))
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
    let mut servers = sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, COALESCE(t.cnt, 0) AS tools_count
           FROM mcp_servers s
           LEFT JOIN (SELECT server_id, COUNT(*) AS cnt FROM mcp_tools WHERE is_active = true GROUP BY server_id) t
             ON t.server_id = s.id
           ORDER BY s.created_at DESC"#,
    )
    .fetch_all(&state.db)
    .await?;

    // Attach lifetime call counts from ClickHouse (mcp_logs) — best-effort:
    // if CH is unavailable we simply leave the counter at 0.
    if super::clickhouse_util::ch_available(&state)
        && let Ok(ch) = super::clickhouse_util::ch_client(&state)
    {
        #[derive(clickhouse::Row, serde::Deserialize)]
        struct CallRow {
            server_id: String,
            calls: u64,
        }
        // Read from the pre-aggregated mcp_server_call_counts table —
        // SummingMergeTree, fed by the mcp_server_call_counts_mv MV on
        // mcp_logs. This is O(number_of_servers) merged rows instead of
        // scanning the full mcp_logs retention window per request.
        let rows = ch
            .query(
                "SELECT server_id, toUInt64(sum(calls)) AS calls
                 FROM mcp_server_call_counts
                 GROUP BY server_id",
            )
            .fetch_all::<CallRow>()
            .await
            .unwrap_or_default();
        let mut lookup = std::collections::HashMap::<String, i64>::new();
        for r in rows {
            lookup.insert(r.server_id, r.calls as i64);
        }
        for s in &mut servers {
            s.call_count = lookup.get(&s.id.to_string()).copied().unwrap_or(0);
        }
    }

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

    // Resolve + validate namespace prefix (explicit, or derived from name).
    let namespace_prefix = normalize_namespace_prefix(
        req.namespace_prefix.as_deref().filter(|s| !s.is_empty()),
        &req.name,
    )?;

    // SSRF prevention: validate endpoint_url
    think_watch_common::validation::validate_url(&req.endpoint_url)?;

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
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(
            &http_detect,
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
        r#"INSERT INTO mcp_servers (name, namespace_prefix, description, endpoint_url, transport_type, auth_type, auth_secret_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&namespace_prefix)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(&transport_type)
    .bind(&req.auth_type)
    .bind(&auth_encrypted)
    .bind({
        let mut config = serde_json::json!({});
        if let Some(ref headers) = req.custom_headers {
            think_watch_common::validation::validate_custom_headers(headers)?;
            config["custom_headers"] = serde_json::to_value(headers).unwrap_or_default();
        }
        if let Some(ttl) = req.cache_ttl_secs {
            config["cache_ttl_secs"] = serde_json::json!(ttl);
        }
        config
    })
    .fetch_one(&state.db)
    .await
    .map_err(map_mcp_server_unique_violation)?;

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
        let http = (**state.http_client.load()).clone();
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
    pub namespace_prefix: Option<String>,
    /// PATCH semantics: absent = unchanged, JSON `null` = clear,
    /// JSON string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    pub endpoint_url: Option<String>,
    pub transport_type: Option<String>,
    /// Same PATCH semantics as `description`. Sending `null` clears
    /// the auth requirement so the server can be probed unauthenticated.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub auth_type: Option<Option<String>>,
    pub auth_secret: Option<String>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    /// Values may contain `{{user_id}}` / `{{user_email}}` template variables.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    /// Per-server response cache TTL in seconds. `None` = use global default.
    /// `0` = disable caching for this server.
    pub cache_ttl_secs: Option<u64>,
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
    // None = field absent (preserve), Some(None) = clear, Some(Some) = set.
    let description: Option<&str> = match &req.description {
        None => existing.description.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let endpoint_url = req
        .endpoint_url
        .as_deref()
        .unwrap_or(&existing.endpoint_url);
    let auth_type: Option<&str> = match &req.auth_type {
        None => existing.auth_type.as_deref(),
        Some(inner) => inner.as_deref(),
    };

    // Resolve new namespace_prefix: explicit override > existing value.
    // Validated only when the caller provided one (otherwise keep existing).
    let namespace_prefix = match req.namespace_prefix.as_deref() {
        Some(p) if !p.is_empty() => normalize_namespace_prefix(Some(p), name)?,
        _ => existing.namespace_prefix.clone(),
    };

    if req.endpoint_url.is_some() {
        think_watch_common::validation::validate_url(endpoint_url)?;
    }

    // Auto-detect transport type when endpoint changes, otherwise keep existing
    let transport_type = if req.endpoint_url.is_some() {
        let auth_hdr = build_auth_probe_header(auth_type, req.auth_secret.as_deref());
        let auth_ref = auth_hdr.as_ref().map(|(n, v)| (n.as_str(), v.as_str()));
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(
            &http_detect,
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

    // Merge custom_headers + cache_ttl into existing config_json
    let mut config_json = existing.config_json.clone();
    if let Some(ref headers) = req.custom_headers {
        think_watch_common::validation::validate_custom_headers(headers)?;
        config_json["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
    }
    if let Some(ttl) = req.cache_ttl_secs {
        config_json["cache_ttl_secs"] = serde_json::json!(ttl);
    }

    let updated = sqlx::query_as::<_, McpServer>(
        r#"UPDATE mcp_servers SET name = $2, namespace_prefix = $3, description = $4, endpoint_url = $5,
           transport_type = $6, auth_type = $7, auth_secret_encrypted = $8, config_json = $9
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(&namespace_prefix)
    .bind(description)
    .bind(endpoint_url)
    .bind(transport_type)
    .bind(auth_type)
    .bind(&auth_encrypted)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await
    .map_err(map_mcp_server_unique_violation)?;

    // Evict any cached connection first — the pool keys by id, so a
    // changed endpoint URL needs a fresh connection.
    state.mcp_pool.load().remove(id).await;

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
    state.mcp_pool.load().remove(id).await;

    state.audit.log(
        auth_user
            .audit("mcp_server.deleted")
            .resource("mcp_server")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

/// Translate PostgreSQL unique-constraint violations on `mcp_servers` into
/// user-facing conflict errors, so the UI shows "already in use" instead of
/// a generic 500. Other sqlx errors fall through unchanged.
fn map_mcp_server_unique_violation(e: sqlx::Error) -> AppError {
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

// `normalize_namespace_prefix` and `build_auth_probe_header` moved
// to `super::mcp_shared` — see the re-export near the top of this
// file.
