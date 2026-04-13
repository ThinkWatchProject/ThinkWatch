//! MCP gateway runtime — startup loaders, tool discovery, auth resolution,
//! and the periodic health-check loop.
//!
//! Extracted from `app.rs` so that file stays focused on the Axum router
//! wiring. Everything in here works against the shared `AppState`'s MCP
//! sub-fields (`mcp_registry`, `mcp_pool`, `mcp_circuit_breakers`,
//! `http_client`, `db`, `config`).

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
// Auth header resolution
// ---------------------------------------------------------------------------

/// Decrypt the `auth_secret_encrypted` column for an MCP server and shape it
/// into a `(header_name, header_value)` tuple based on `auth_type`.
///
/// Supported `auth_type`:
/// - `bearer`     → `Authorization: Bearer <secret>`
/// - `api_key`    → `X-API-Key: <secret>`
/// - `header`     → secret is `name:value` literal
///
/// Returns `Ok(None)` when the server has no auth configured. Returns `Err`
/// only if decryption itself fails (so the caller can decide whether to
/// abort registration or proceed with a missing header).
pub fn resolve_mcp_auth_header(
    server: &think_watch_common::models::McpServer,
    encryption_key: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let (Some(auth_type), Some(encrypted)) = (
        server.auth_type.as_deref(),
        server.auth_secret_encrypted.as_ref(),
    ) else {
        return Ok(None);
    };

    let key = think_watch_common::crypto::parse_encryption_key(encryption_key)
        .map_err(|e| anyhow::anyhow!("invalid encryption key: {e}"))?;
    let secret_bytes = think_watch_common::crypto::decrypt(encrypted, &key)
        .map_err(|e| anyhow::anyhow!("failed to decrypt auth secret: {e}"))?;
    let secret = String::from_utf8(secret_bytes)
        .map_err(|e| anyhow::anyhow!("auth secret is not valid UTF-8: {e}"))?;

    Ok(match auth_type {
        "bearer" => Some(("Authorization".to_string(), format!("Bearer {secret}"))),
        "api_key" => Some(("X-API-Key".to_string(), secret)),
        "header" => {
            // `name:value` literal — split on the first `:`.
            if let Some((name, value)) = secret.split_once(':') {
                Some((name.trim().to_string(), value.trim().to_string()))
            } else {
                tracing::warn!(
                    mcp_server = %server.name,
                    "auth_type=header expected `name:value`, ignoring"
                );
                None
            }
        }
        other => {
            tracing::warn!(
                mcp_server = %server.name,
                auth_type = other,
                "unknown MCP auth_type, no header attached"
            );
            None
        }
    })
}

// ---------------------------------------------------------------------------
// RegisteredServer construction
// ---------------------------------------------------------------------------

/// Build a `RegisteredServer` from a Postgres `mcp_servers` row, including
/// its previously discovered tools and decrypted auth header. Reused by the
/// startup loader and the CRUD sync paths.
pub async fn build_registered_server(
    db: &PgPool,
    server: &think_watch_common::models::McpServer,
    encryption_key: &str,
) -> anyhow::Result<think_watch_mcp_gateway::registry::RegisteredServer> {
    use think_watch_mcp_gateway::registry::{McpToolInfo, RegisteredServer};

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

    // Resolve auth — log and continue if the secret can't be decrypted, so a
    // single broken row doesn't take down the gateway.
    let auth_header = match resolve_mcp_auth_header(server, encryption_key) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                mcp_server = %server.name,
                error = %e,
                "Failed to resolve MCP auth header — proceeding without auth"
            );
            None
        }
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

    Ok(RegisteredServer {
        id: server.id,
        name: server.name.clone(),
        endpoint_url: server.endpoint_url.clone(),
        transport_type: parse_transport_type(&server.transport_type),
        tools,
        status: parse_server_status(&server.status),
        last_health_check: server.last_health_check,
        auth_header,
        custom_headers,
    })
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
/// Returns the number of tools discovered. Used by:
/// - the startup loader (best effort, non-blocking),
/// - the create/update CRUD handlers (so adding a server in the UI also
///   discovers its tools immediately),
/// - the on-demand `discover_tools` admin handler.
pub async fn discover_and_persist_tools(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
    encryption_key: &str,
) -> anyhow::Result<usize> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut req = http
        .post(&server.endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);

    if let Some((name, value)) = resolve_mcp_auth_header(server, encryption_key)? {
        req = req.header(name, value);
    }

    let resp = req.send().await?;
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
    // Strict: require a `result` field. The previous fallback to the
    // whole JSON body silently swallowed malformed responses (a server
    // that returned `{"error": ...}` without `result` would still appear
    // to "succeed" with garbage-shaped data).
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
        "UPDATE mcp_servers SET status = 'connected', last_health_check = now() WHERE id = $1",
    )
    .bind(server.id)
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
        let http = state.http_client.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            match discover_and_persist_tools(&db, &http, &server, &key).await {
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
    interval_secs: u64,
) {
    let checker = think_watch_mcp_gateway::health::HealthChecker::new(pool);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // First tick fires immediately; skip it so we don't probe the
        // moment the server starts (avoids piling on top of the startup
        // discovery burst).
        tick.tick().await;
        loop {
            tick.tick().await;
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
        }
    });
}

/// Extract JSON from an SSE response body by scanning `data:` lines.
fn parse_sse_json(text: &str) -> anyhow::Result<serde_json::Value> {
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
