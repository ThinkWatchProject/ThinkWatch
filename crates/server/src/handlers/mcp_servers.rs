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

// `probe_mcp_endpoint`, `McpProbeOutcome`, `McpToolSummary`, and
// `normalize_namespace_prefix` live in `super::mcp_shared` so
// `mcp_store` (and any future caller) can reach them without
// reaching across handlers.
pub use super::mcp_shared::{McpToolSummary, normalize_namespace_prefix, probe_mcp_endpoint};

// ---------------------------------------------------------------------------
// Test MCP server connection — anonymous probe via JSON-RPC tools/list
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct TestMcpServerRequest {
    pub endpoint_url: String,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TestMcpServerResponse {
    pub success: bool,
    /// True when the server responded with 401/403 — reachable, but the
    /// anonymous probe wasn't permitted to enumerate tools. The admin
    /// "test" button treats this as a soft success since per-user auth
    /// happens later when an end user connects via /connections.
    pub requires_auth: bool,
    pub message: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolSummary>>,
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

    let http = state.http_client.load();
    let outcome = probe_mcp_endpoint(&http, &req.endpoint_url, req.custom_headers.as_ref()).await;

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
    // Anonymous probe: 401/403 is the *expected* response for OAuth /
    // static-token MCPs — surface as success so the wizard's Save isn't
    // blocked. The frontend uses `requires_auth` to render an explanatory
    // banner ("auth will be validated on first connection").
    let success = outcome.success || outcome.requires_auth;
    Ok(Json(TestMcpServerResponse {
        success,
        requires_auth: outcome.requires_auth,
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

/// Encrypt the OAuth client_secret with the configured AES-GCM key.
/// Returns Ok(None) if no secret was provided.
///
/// Pub so `mcp_store::install_template` can reuse the same encryption
/// path when admins supply credentials at install time.
pub fn encrypt_client_secret(
    plain: Option<&str>,
    encryption_key: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    let Some(secret) = plain.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let key = crypto::parse_encryption_key(encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
    let bytes = crypto::encrypt(secret.as_bytes(), &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?;
    Ok(Some(bytes))
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

    // Encrypt the OAuth client_secret if one was supplied.
    let oauth_client_secret_encrypted = encrypt_client_secret(
        req.oauth_client_secret.as_deref(),
        &state.config.encryption_key,
    )?;

    // Auto-detect transport type if not explicitly specified. The
    // detector probes anonymously — upstreams that gate detection
    // behind auth will fall back to streamable_http, which is the
    // default for MCP servers.
    let transport_type = if let Some(ref tt) = req.transport_type {
        tt.clone()
    } else {
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(
            &http_detect,
            &req.endpoint_url,
            None,
        )
        .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => "streamable_http".to_owned(),
        }
    };

    let allow_static_token = req.allow_static_token.unwrap_or(false);
    let oauth_scopes = req.oauth_scopes.unwrap_or_default();

    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (
               name, namespace_prefix, description, endpoint_url, transport_type,
               oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
               oauth_revocation_endpoint, oauth_userinfo_endpoint,
               oauth_client_id, oauth_client_secret_encrypted,
               oauth_scopes, allow_static_token, static_token_help_url,
               config_json
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
           RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&namespace_prefix)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(&transport_type)
    .bind(&req.oauth_issuer)
    .bind(&req.oauth_authorization_endpoint)
    .bind(&req.oauth_token_endpoint)
    .bind(&req.oauth_revocation_endpoint)
    .bind(&req.oauth_userinfo_endpoint)
    .bind(&req.oauth_client_id)
    .bind(&oauth_client_secret_encrypted)
    .bind(&oauth_scopes)
    .bind(allow_static_token)
    .bind(&req.static_token_help_url)
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
        let server_id = server.id;
        let db_for_err = state.db.clone();
        tokio::spawn(async move {
            match crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server).await {
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
    /// PATCH semantics: absent = unchanged, JSON `null` = clear,
    /// JSON string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_issuer: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_authorization_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_token_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_revocation_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_userinfo_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_client_id: Option<Option<String>>,
    /// Plaintext client secret; sending an empty string clears it.
    /// Encrypted at rest before persisting.
    pub oauth_client_secret: Option<String>,
    pub oauth_scopes: Option<Vec<String>>,
    pub allow_static_token: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub static_token_help_url: Option<Option<String>>,
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
    // PATCH semantics for nullable strings: None = absent (preserve),
    // Some(None) = JSON null (clear), Some(Some(s)) = replace.
    let description: Option<&str> = match &req.description {
        None => existing.description.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let endpoint_url = req
        .endpoint_url
        .as_deref()
        .unwrap_or(&existing.endpoint_url);
    let oauth_issuer: Option<&str> = match &req.oauth_issuer {
        None => existing.oauth_issuer.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_authorization_endpoint: Option<&str> = match &req.oauth_authorization_endpoint {
        None => existing.oauth_authorization_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_token_endpoint: Option<&str> = match &req.oauth_token_endpoint {
        None => existing.oauth_token_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_revocation_endpoint: Option<&str> = match &req.oauth_revocation_endpoint {
        None => existing.oauth_revocation_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_userinfo_endpoint: Option<&str> = match &req.oauth_userinfo_endpoint {
        None => existing.oauth_userinfo_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_client_id: Option<&str> = match &req.oauth_client_id {
        None => existing.oauth_client_id.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let static_token_help_url: Option<&str> = match &req.static_token_help_url {
        None => existing.static_token_help_url.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let allow_static_token = req
        .allow_static_token
        .unwrap_or(existing.allow_static_token);
    let oauth_scopes = req
        .oauth_scopes
        .clone()
        .unwrap_or_else(|| existing.oauth_scopes.clone());

    // Resolve new namespace_prefix: explicit override > existing value.
    let namespace_prefix = match req.namespace_prefix.as_deref() {
        Some(p) if !p.is_empty() => normalize_namespace_prefix(Some(p), name)?,
        _ => existing.namespace_prefix.clone(),
    };

    if req.endpoint_url.is_some() {
        think_watch_common::validation::validate_url(endpoint_url)?;
    }

    // Auto-detect transport type when endpoint changes; otherwise the
    // value the server is already storing stays.
    let transport_type = if req.endpoint_url.is_some() {
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(&http_detect, endpoint_url, None)
            .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => existing.transport_type.clone(),
        }
    } else {
        existing.transport_type.clone()
    };

    // Encrypt the OAuth client_secret iff the caller supplied one;
    // empty string ⇒ clear, absent ⇒ keep existing.
    let oauth_client_secret_encrypted = match req.oauth_client_secret.as_deref() {
        Some("") => None,
        Some(s) => encrypt_client_secret(Some(s), &state.config.encryption_key)?,
        None => existing.oauth_client_secret_encrypted.clone(),
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
        r#"UPDATE mcp_servers SET
              name = $2, namespace_prefix = $3, description = $4, endpoint_url = $5,
              transport_type = $6,
              oauth_issuer = $7, oauth_authorization_endpoint = $8,
              oauth_token_endpoint = $9, oauth_revocation_endpoint = $10,
              oauth_userinfo_endpoint = $11,
              oauth_client_id = $12, oauth_client_secret_encrypted = $13,
              oauth_scopes = $14, allow_static_token = $15, static_token_help_url = $16,
              config_json = $17
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(&namespace_prefix)
    .bind(description)
    .bind(endpoint_url)
    .bind(transport_type)
    .bind(oauth_issuer)
    .bind(oauth_authorization_endpoint)
    .bind(oauth_token_endpoint)
    .bind(oauth_revocation_endpoint)
    .bind(oauth_userinfo_endpoint)
    .bind(oauth_client_id)
    .bind(&oauth_client_secret_encrypted)
    .bind(&oauth_scopes)
    .bind(allow_static_token)
    .bind(static_token_help_url)
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
