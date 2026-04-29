//! MCP gateway runtime — startup loaders, tool discovery, and the
//! periodic health-check loop.
//!
//! Extracted from `app.rs` so that file stays focused on the Axum router
//! wiring. Everything in here works against the shared `AppState`'s MCP
//! sub-fields (`mcp_registry`, `mcp_pool`, `mcp_circuit_breakers`,
//! `http_client`, `db`, `config`).
//!
//! Per-user credentials are NOT resolved here — that's the proxy's job
//! at request time via [`think_watch_mcp_gateway::user_token`]. Probes
//! and health checks make anonymous calls; if a server requires auth
//! they will surface as warnings until a user authorizes.

use sqlx::PgPool;

use crate::app::AppState;

// ---------------------------------------------------------------------------
// DB row → runtime enum mapping
// ---------------------------------------------------------------------------

/// Map a Postgres `mcp_servers.transport_type` string to the runtime enum.
/// The schema currently only supports `streamable_http`; unknown values fall
/// back to that and emit a warning.
fn parse_transport_type(s: &str) -> think_watch_mcp_gateway::registry::TransportType {
    use think_watch_mcp_gateway::registry::TransportType;
    match s {
        "streamable_http" => TransportType::StreamableHttp,
        other => {
            tracing::warn!(
                transport = other,
                "Unknown MCP transport_type, defaulting to streamable_http"
            );
            TransportType::StreamableHttp
        }
    }
}

/// Map the textual `status` column to the runtime `ServerStatus` enum.
fn parse_server_status(s: &str) -> think_watch_mcp_gateway::registry::ServerStatus {
    use think_watch_mcp_gateway::registry::ServerStatus;
    match s {
        "connected" => ServerStatus::Connected,
        "disconnected" => ServerStatus::Disconnected,
        _ => ServerStatus::Unknown,
    }
}

// ---------------------------------------------------------------------------
// RegisteredServer construction
// ---------------------------------------------------------------------------

/// Build a `RegisteredServer` from a Postgres `mcp_servers` row. Decrypts
/// the OAuth `client_secret` once so the per-request hot path stays free
/// of crypto. Reused by the startup loader and the CRUD sync paths.
pub async fn build_registered_server(
    db: &PgPool,
    server: &think_watch_common::models::McpServer,
    encryption_key: &str,
) -> anyhow::Result<think_watch_mcp_gateway::registry::RegisteredServer> {
    use think_watch_mcp_gateway::registry::{McpToolInfo, RegisteredServer};
    use think_watch_mcp_gateway::user_token::OAuthClientCfg;

    let tool_rows = sqlx::query_as::<_, think_watch_common::models::McpTool>(
        "SELECT * FROM mcp_tools WHERE server_id = $1 AND is_active = true",
    )
    .bind(server.id)
    .fetch_all(db)
    .await?;

    let tools = tool_rows
        .into_iter()
        .map(|t| McpToolInfo {
            name: t.tool_name,
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect();

    // Resolve the OAuth client config when present. A row qualifies as
    // OAuth-capable when it has BOTH the token endpoint and a client
    // id+secret pair — anything missing is treated as "OAuth not
    // configured" (the resolver will then either fall through to
    // static-token mode or surface NeedsUserCredentials).
    let oauth_cfg = match (
        server.oauth_token_endpoint.as_deref(),
        server.oauth_client_id.as_deref(),
        server.oauth_client_secret_encrypted.as_ref(),
    ) {
        (Some(token_endpoint), Some(client_id), Some(encrypted)) => {
            match decrypt_client_secret(encrypted, encryption_key) {
                Ok(client_secret) => Some(OAuthClientCfg {
                    token_endpoint: token_endpoint.to_string(),
                    authorization_endpoint: server.oauth_authorization_endpoint.clone(),
                    client_id: client_id.to_string(),
                    client_secret,
                    scopes: server.oauth_scopes.clone(),
                }),
                Err(e) => {
                    tracing::error!(
                        mcp_server = %server.name,
                        error = %e,
                        "Failed to decrypt MCP OAuth client_secret — skipping OAuth registration"
                    );
                    None
                }
            }
        }
        _ => None,
    };

    // Parse custom headers from config_json.custom_headers (key→value map)
    // Values may contain {{user_id}} / {{user_email}} template variables.
    let custom_headers: Vec<(String, String)> = server
        .config_json
        .get("custom_headers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // Detect whether any custom header forwards per-user identity —
    // if so, responses vary by caller and must not be cached.
    let forwards_user_identity = custom_headers
        .iter()
        .any(|(_, v)| v.contains("{{user_id}}") || v.contains("{{user_email}}"));

    // Per-server cache TTL override from config_json.cache_ttl_secs.
    let cache_ttl_secs = server
        .config_json
        .get("cache_ttl_secs")
        .and_then(|v| v.as_u64());

    Ok(RegisteredServer {
        id: server.id,
        name: server.name.clone(),
        namespace_prefix: server.namespace_prefix.clone(),
        endpoint_url: server.endpoint_url.clone(),
        transport_type: parse_transport_type(&server.transport_type),
        tools,
        status: parse_server_status(&server.status),
        last_health_check: server.last_health_check,
        oauth_cfg,
        allow_static_token: server.allow_static_token,
        custom_headers,
        cache_ttl_secs,
        forwards_user_identity,
    })
}

fn decrypt_client_secret(encrypted: &[u8], encryption_key: &str) -> anyhow::Result<String> {
    let key = think_watch_common::crypto::parse_encryption_key(encryption_key)
        .map_err(|e| anyhow::anyhow!("invalid encryption key: {e}"))?;
    let bytes = think_watch_common::crypto::decrypt(encrypted, &key)
        .map_err(|e| anyhow::anyhow!("failed to decrypt client_secret: {e}"))?;
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("client_secret is not valid UTF-8: {e}"))
}

// ---------------------------------------------------------------------------
// Tool auto-discovery
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct McpToolDef {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct McpToolsListResult {
    tools: Vec<McpToolDef>,
}

/// Live-fetch the tool list from an MCP server via JSON-RPC `tools/list`,
/// upsert into the `mcp_tools` table (deactivating ones that disappeared),
/// and update the server's `status` + `last_health_check` columns.
///
/// Probes are anonymous — they don't carry per-user credentials. For
/// upstreams that gate `tools/list` behind auth this will fail, the
/// server is marked disconnected, and the cached tool catalog stays
/// whatever the most recent successful probe produced.
pub async fn discover_and_persist_tools(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
) -> anyhow::Result<usize> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let resp = http
        .post(&server.endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        // Mark the server disconnected and bail.
        let _ = sqlx::query(
            "UPDATE mcp_servers SET status = 'disconnected', last_health_check = now() WHERE id = $1",
        )
        .bind(server.id)
        .execute(db)
        .await;
        anyhow::bail!("MCP server returned HTTP {}", resp.status());
    }

    // Handle both plain JSON and SSE response formats
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let json: serde_json::Value = if content_type.contains("text/event-stream") {
        let text = resp.text().await?;
        parse_sse_json(&text)?
    } else {
        resp.json().await?
    };
    // Strict: require a `result` field so malformed responses are
    // caught instead of silently treated as success.
    let result = json
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("MCP tools/list response missing `result` field"))?
        .clone();
    let parsed: McpToolsListResult = serde_json::from_value(result)?;

    // Atomic: deactivate-then-upsert wrapped in a transaction so concurrent
    // readers never observe a temporarily-empty tool list. Without the
    // transaction, the deactivate above would be visible to other
    // connections before the upserts re-enable the live tools.
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE mcp_tools SET is_active = false WHERE server_id = $1")
        .bind(server.id)
        .execute(&mut *tx)
        .await?;

    // Snapshot the discovered list into mcp_servers.cached_tools_jsonb
    // so users that haven't authorized yet still see the catalog.
    let cached_tools_jsonb = serde_json::json!({
        "tools": parsed.tools.iter().map(|t| serde_json::json!({
            "name": t.name,
            "description": t.description,
            "inputSchema": t.input_schema,
        })).collect::<Vec<_>>(),
    });

    for tool in &parsed.tools {
        sqlx::query(
            r#"INSERT INTO mcp_tools (server_id, tool_name, description, input_schema, is_active, discovered_at)
               VALUES ($1, $2, $3, $4, true, now())
               ON CONFLICT (server_id, tool_name)
               DO UPDATE SET description = $3, input_schema = $4, is_active = true, discovered_at = now()"#,
        )
        .bind(server.id)
        .bind(&tool.name)
        .bind(&tool.description)
        .bind(&tool.input_schema)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE mcp_servers SET status = 'connected', last_health_check = now(),
            cached_tools_jsonb = $2, cached_tools_at = now() WHERE id = $1",
    )
    .bind(server.id)
    .bind(&cached_tools_jsonb)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(parsed.tools.len())
}

// ---------------------------------------------------------------------------
// Startup loader + background health loop
// ---------------------------------------------------------------------------

/// Load all MCP servers from the database into the in-memory registry. Each
/// server's previously-discovered tools (from `mcp_tools`) are attached so
/// the gateway can serve `tools/list` immediately, before the first live
/// discovery completes. After the registry is populated, fire off a
/// best-effort `tools/list` against each server in the background to refresh
/// the cached metadata.
pub async fn load_mcp_servers_into_registry(
    state: &AppState,
    registry: &think_watch_mcp_gateway::registry::Registry,
) -> anyhow::Result<()> {
    let servers =
        sqlx::query_as::<_, think_watch_common::models::McpServer>("SELECT * FROM mcp_servers")
            .fetch_all(&state.db)
            .await?;

    let encryption_key = state.config.encryption_key.clone();
    for server in &servers {
        match build_registered_server(&state.db, server, &encryption_key).await {
            Ok(registered) => {
                let tool_count = registered.tools.len();
                registry.register(registered).await;
                tracing::info!(
                    mcp_server = %server.name,
                    server_id = %server.id,
                    tools = tool_count,
                    "MCP server loaded"
                );
            }
            Err(e) => {
                tracing::error!(
                    mcp_server = %server.name,
                    error = %e,
                    "Failed to load MCP server tools"
                );
            }
        }
    }

    tracing::info!(
        total_mcp_servers = servers.len(),
        "All MCP servers loaded into registry"
    );

    // Kick off background tool discovery for every loaded server. We don't
    // block startup on this — failures only mean stale tool metadata until
    // the next refresh, which is fine.
    for server in servers {
        let db = state.db.clone();
        let key = encryption_key.clone();
        let http = (**state.http_client.load()).clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            match discover_and_persist_tools(&db, &http, &server).await {
                Ok(n) => {
                    tracing::info!(
                        mcp_server = %server.name,
                        tools = n,
                        "MCP tool discovery refreshed"
                    );
                    // Re-build the in-memory entry so the new tool list is
                    // visible to the gateway without waiting for restart.
                    if let Ok(updated) = build_registered_server(&db, &server, &key).await {
                        registry.register(updated).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "MCP tool discovery failed (using cached tools)"
                    );
                }
            }
        });
    }

    Ok(())
}

/// Background health-check loop. Probes every registered MCP server every
/// `interval_secs` and writes the result back to:
/// - the in-memory `Registry` (so `tools/list` knows what's reachable)
/// - the `mcp_servers.status` + `last_health_check` columns (so the admin
///   UI surfaces real status without a manual refresh).
///
/// This replaces the `HealthChecker::start_background_checks` API for the
/// server crate because that one only updates the in-memory registry and
/// has no DB access.
pub fn spawn_mcp_health_loop(
    state: AppState,
    registry: think_watch_mcp_gateway::registry::Registry,
    pool: think_watch_mcp_gateway::pool::ConnectionPool,
) {
    let checker = think_watch_mcp_gateway::health::HealthChecker::new(pool);
    tokio::spawn(async move {
        // Skip the immediate-fire first tick so we don't pile probes on
        // top of the startup discovery burst. Cadence is read from
        // DynamicConfig (`mcp.health_interval_secs`) before each sleep,
        // so changes via the settings UI take effect within one tick
        // — no restart needed.
        tokio::time::sleep(std::time::Duration::from_secs(
            state.dynamic_config.mcp_health_interval_secs().await,
        ))
        .await;
        loop {
            let servers = registry.list().await;
            for server in &servers {
                let health = checker.check_server(server).await;
                let new_status = if health.error.is_none() {
                    think_watch_mcp_gateway::registry::ServerStatus::Connected
                } else {
                    think_watch_mcp_gateway::registry::ServerStatus::Disconnected
                };
                registry.update_status(server.id, new_status.clone()).await;

                // Mirror the runtime status + last error into Postgres so
                // the admin UI doesn't depend on a fresh process being up.
                let status_str = match new_status {
                    think_watch_mcp_gateway::registry::ServerStatus::Connected => "connected",
                    think_watch_mcp_gateway::registry::ServerStatus::Disconnected => "disconnected",
                    think_watch_mcp_gateway::registry::ServerStatus::Unknown => "unknown",
                };
                if let Err(e) = sqlx::query(
                    "UPDATE mcp_servers SET status = $1, last_health_check = now(), last_error = $2 WHERE id = $3",
                )
                .bind(status_str)
                .bind(health.error.clone())
                .bind(server.id)
                .execute(&state.db)
                .await
                {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "Failed to write back MCP server health status"
                    );
                }
            }
            // Re-read cadence each iteration so settings UI changes
            // take effect immediately on the next probe round.
            let secs = state.dynamic_config.mcp_health_interval_secs().await;
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

/// Extract JSON from an SSE response body by scanning `data:` lines.
pub fn parse_sse_json(text: &str) -> anyhow::Result<serde_json::Value> {
    let mut data_buf = String::new();

    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim_start();
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(payload);
        } else if line.is_empty() && !data_buf.is_empty() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data_buf) {
                return Ok(val);
            }
            data_buf.clear();
        }
    }

    if !data_buf.is_empty()
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&data_buf)
    {
        return Ok(val);
    }

    anyhow::bail!("No valid JSON found in SSE stream")
}
