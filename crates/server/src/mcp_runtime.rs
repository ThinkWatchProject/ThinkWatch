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

    let oauth_cfg = build_oauth_cfg(server, encryption_key);

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

    let credential_owner =
        think_watch_mcp_gateway::user_token::CredentialOwner::parse(&server.credential_owner);
    let auth_shape = think_watch_mcp_gateway::user_token::AuthShape::parse(&server.auth_shape);

    let cache_scope = determine_cache_scope(auth_shape, &custom_headers, credential_owner);

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
        auth_shape,
        credential_owner,
        auth_header_name: server.auth_header_name.clone(),
        auth_value_template: server.auth_value_template.clone(),
        custom_headers,
        cache_ttl_secs,
        cache_scope,
    })
}

/// Decide which [`ServerCacheScope`] applies based on the persisted
/// auth config.
///
/// Rules:
///   - Per-user template headers (`{{user_id}}` / `{{user_email}}`)
///     always force `PerCaller` — the upstream sees per-caller
///     identity in the headers regardless of who owns the credential.
///   - `admin_shared` ⇒ one bearer for everyone ⇒ `Global` (unless
///     overridden by template headers above).
///   - `per_user` + non-anonymous shape ⇒ `PerCaller`.
///   - `anonymous` ⇒ `Global`.
///
/// Pure function — exposed for unit testing without spinning up a DB.
pub fn determine_cache_scope(
    auth_shape: think_watch_mcp_gateway::user_token::AuthShape,
    custom_headers: &[(String, String)],
    credential_owner: think_watch_mcp_gateway::user_token::CredentialOwner,
) -> think_watch_mcp_gateway::registry::ServerCacheScope {
    use think_watch_mcp_gateway::registry::ServerCacheScope;
    use think_watch_mcp_gateway::user_token::{AuthShape, CredentialOwner};

    let header_per_caller = custom_headers
        .iter()
        .any(|(_, v)| v.contains("{{user_id}}") || v.contains("{{user_email}}"));
    if header_per_caller {
        return ServerCacheScope::PerCaller;
    }

    match (credential_owner, auth_shape) {
        // Anonymous: no credential at all, response shape is identical
        // across callers.
        (_, AuthShape::Anonymous) => ServerCacheScope::Global,
        // Admin-shared bearer ⇒ every caller looks identical to the
        // upstream regardless of OAuth/static.
        (CredentialOwner::AdminShared, _) => ServerCacheScope::Global,
        // Per-user with any auth ⇒ per-caller upstream identity.
        (CredentialOwner::PerUser, _) => ServerCacheScope::PerCaller,
    }
}

#[cfg(test)]
mod cache_scope_tests {
    use super::determine_cache_scope;
    use think_watch_mcp_gateway::registry::ServerCacheScope;
    use think_watch_mcp_gateway::user_token::{AuthShape, CredentialOwner};

    fn h(k: &str, v: &str) -> Vec<(String, String)> {
        vec![(k.to_string(), v.to_string())]
    }

    #[test]
    fn anonymous_is_global_regardless_of_owner() {
        for owner in [CredentialOwner::PerUser, CredentialOwner::AdminShared] {
            let s = determine_cache_scope(AuthShape::Anonymous, &[], owner);
            assert_eq!(s, ServerCacheScope::Global);
        }
    }

    #[test]
    fn fixed_service_to_service_header_is_global() {
        // Anonymous shape with a fixed `X-API-Key: <secret>` custom
        // header — same secret for every caller, response is identical.
        let s = determine_cache_scope(
            AuthShape::Anonymous,
            &h("X-API-Key", "fixed-secret"),
            CredentialOwner::PerUser,
        );
        assert_eq!(s, ServerCacheScope::Global);
    }

    #[test]
    fn user_id_template_header_forces_per_caller() {
        let s = determine_cache_scope(
            AuthShape::Anonymous,
            &h("X-User-Id", "{{user_id}}"),
            CredentialOwner::PerUser,
        );
        assert_eq!(s, ServerCacheScope::PerCaller);
    }

    #[test]
    fn per_user_oauth_is_per_caller() {
        let s = determine_cache_scope(AuthShape::OAuth, &[], CredentialOwner::PerUser);
        assert_eq!(s, ServerCacheScope::PerCaller);
    }

    #[test]
    fn per_user_static_is_per_caller() {
        let s = determine_cache_scope(AuthShape::Static, &[], CredentialOwner::PerUser);
        assert_eq!(s, ServerCacheScope::PerCaller);
    }

    #[test]
    fn admin_shared_oauth_is_global() {
        let s = determine_cache_scope(AuthShape::OAuth, &[], CredentialOwner::AdminShared);
        assert_eq!(s, ServerCacheScope::Global);
    }

    #[test]
    fn admin_shared_static_is_global() {
        let s = determine_cache_scope(AuthShape::Static, &[], CredentialOwner::AdminShared);
        assert_eq!(s, ServerCacheScope::Global);
    }

    #[test]
    fn admin_shared_with_user_id_header_still_per_caller() {
        let s = determine_cache_scope(
            AuthShape::OAuth,
            &h("X-User-Id", "{{user_id}}"),
            CredentialOwner::AdminShared,
        );
        assert_eq!(s, ServerCacheScope::PerCaller);
    }
}

/// Resolve the upstream OAuth client config from a server row. Returns
/// `None` when token_endpoint or client_id is missing — those two are
/// strictly required. `client_secret` is optional: AS that advertise
/// public-client support (Feishu, Cloudflare, etc.) accept PKCE-only
/// token requests, and admin will leave the secret blank for them.
/// Decryption failures degrade to `None` so a corrupted row doesn't
/// break the gateway hard.
pub fn build_oauth_cfg(
    server: &think_watch_common::models::McpServer,
    encryption_key: &str,
) -> Option<think_watch_mcp_gateway::user_token::OAuthClientCfg> {
    use think_watch_mcp_gateway::user_token::OAuthClientCfg;
    let token_endpoint = server.oauth_token_endpoint.as_deref()?.to_string();
    let client_id = server.oauth_client_id.as_deref()?.to_string();
    let client_secret = match server.oauth_client_secret_encrypted.as_ref() {
        Some(encrypted) => match decrypt_client_secret(encrypted, encryption_key) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(
                    mcp_server = %server.name,
                    error = %e,
                    "Failed to decrypt MCP OAuth client_secret"
                );
                return None;
            }
        },
        None => None,
    };
    Some(OAuthClientCfg {
        token_endpoint,
        authorization_endpoint: server.oauth_authorization_endpoint.clone(),
        client_id,
        client_secret,
        scopes: server.oauth_scopes.clone(),
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
/// Outcome of a system-level (anonymous) tool discovery attempt.
///
/// Three terminal states:
/// - `Tools(n)` — anonymous probe succeeded; `mcp_tools` and
///   `mcp_servers.cached_tools_jsonb` were updated with `n` tools.
/// - `AuthRequired` — upstream returned 401/403; we deliberately
///   *did not* write any system-level tool metadata. Per-user
///   discovery (via the gateway's authenticated `tools/list` proxy)
///   is the only path to tool data for this server. The server's
///   `status` column was set to `auth_required`.
/// - `Failed(_)` — network error, 5xx, malformed response, etc.
///
/// Callers MUST distinguish `AuthRequired` from `Failed` because
/// admin-facing surfaces (the manual "rediscover" button) need to
/// show neutral guidance ("authorize a user first") rather than a
/// red error toast for the former.
pub enum SystemDiscoveryOutcome {
    Tools(usize),
    AuthRequired,
    Failed(anyhow::Error),
}

impl SystemDiscoveryOutcome {
    pub fn tools_count(&self) -> usize {
        match self {
            SystemDiscoveryOutcome::Tools(n) => *n,
            _ => 0,
        }
    }
}

pub async fn discover_and_persist_tools(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
) -> SystemDiscoveryOutcome {
    match try_discover_and_persist_tools(db, http, server).await {
        Ok(outcome) => outcome,
        Err(e) => SystemDiscoveryOutcome::Failed(e),
    }
}

async fn try_discover_and_persist_tools(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
) -> anyhow::Result<SystemDiscoveryOutcome> {
    try_discover_and_persist_tools_with_auth(db, http, server, None).await
}

/// Variant that attaches an auth header to the discovery probe. Used
/// by the admin_shared shared-credential write path: after the admin
/// pastes a token (or completes the OAuth flow), the catalog is the
/// same for every caller, so we discover once and write the result
/// straight into the system-level catalog.
pub async fn discover_and_persist_tools_with_auth(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
    auth: Option<(&str, &str)>,
) -> SystemDiscoveryOutcome {
    match try_discover_and_persist_tools_with_auth(db, http, server, auth).await {
        Ok(outcome) => outcome,
        Err(e) => SystemDiscoveryOutcome::Failed(e),
    }
}

async fn try_discover_and_persist_tools_with_auth(
    db: &PgPool,
    http: &reqwest::Client,
    server: &think_watch_common::models::McpServer,
    auth: Option<(&str, &str)>,
) -> anyhow::Result<SystemDiscoveryOutcome> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut req = http
        .post(&server.endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some((name, value)) = auth {
        req = req.header(name, value);
    }
    let resp = req.json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        // 401/403 means the server is reachable but anonymous tool
        // discovery isn't allowed — expected for OAuth / static-token
        // MCPs (e.g. Feishu, GitHub Copilot). Mark `auth_required` and
        // wipe any stale system-level catalog (admin may have flipped a
        // previously-anonymous server to require auth — those rows are
        // now privilege-escalation risk if left visible to all users).
        if status == 401 || status == 403 {
            let mut tx = db.begin().await?;
            sqlx::query(
                "UPDATE mcp_servers SET status = 'auth_required',
                    last_health_check = now(),
                    cached_tools_jsonb = NULL,
                    cached_tools_at = NULL
                 WHERE id = $1",
            )
            .bind(server.id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM mcp_tools WHERE server_id = $1")
                .bind(server.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(SystemDiscoveryOutcome::AuthRequired);
        }
        let _ = sqlx::query(
            "UPDATE mcp_servers SET status = 'disconnected', last_health_check = now()
             WHERE id = $1",
        )
        .bind(server.id)
        .execute(db)
        .await;
        anyhow::bail!("MCP server returned HTTP {}", status);
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

    // Batch upsert via UNNEST. The previous loop did one round-trip
    // per tool — a server with 50 tools meant 50 sequential round-trips
    // inside the same TX. Single statement with array binds collapses
    // that to one.
    if !parsed.tools.is_empty() {
        let names: Vec<&str> = parsed.tools.iter().map(|t| t.name.as_str()).collect();
        let descriptions: Vec<Option<String>> =
            parsed.tools.iter().map(|t| t.description.clone()).collect();
        let input_schemas: Vec<serde_json::Value> = parsed
            .tools
            .iter()
            .map(|t| t.input_schema.clone().unwrap_or(serde_json::Value::Null))
            .collect();
        sqlx::query(
            r#"INSERT INTO mcp_tools (server_id, tool_name, description, input_schema, is_active, discovered_at)
               SELECT $1, name, descr, schema, true, now()
                 FROM UNNEST($2::text[], $3::text[], $4::jsonb[]) AS t(name, descr, schema)
               ON CONFLICT (server_id, tool_name)
               DO UPDATE SET description = EXCLUDED.description,
                             input_schema = EXCLUDED.input_schema,
                             is_active = true,
                             discovered_at = now()"#,
        )
        .bind(server.id)
        .bind(&names)
        .bind(&descriptions)
        .bind(&input_schemas)
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

    Ok(SystemDiscoveryOutcome::Tools(parsed.tools.len()))
}

/// Per-user tool discovery. POSTs `tools/list` with the supplied auth
/// header (already template-substituted by the caller — typically
/// `auth_value_template.replace("{{token}}", access_token)`) and
/// writes the response to `mcp_user_tools(server_id, user_id, ...)`.
/// Never writes to the system-level `mcp_tools` table — see schema
/// comment for why.
///
/// The header is fully caller-supplied — we don't assume `Authorization:
/// Bearer ...`. Per-server `auth_header_name` (`X-API-Key`, `api-key`,
/// ...) and `auth_value_template` (`{{token}}`, `Bearer {{token}}`,
/// ...) configurations all work without this function knowing about
/// them.
///
/// Idempotent: delete-then-upsert in one tx. Best-effort: returns
/// `Ok(0)` and logs a warn on any failure so callers can spawn this
/// without worrying about error propagation breaking the auth flow.
pub async fn discover_user_tools(
    db: &PgPool,
    http: &reqwest::Client,
    endpoint_url: &str,
    server_id: uuid::Uuid,
    user_id: uuid::Uuid,
    auth_header_name: &str,
    auth_header_value: &str,
) -> anyhow::Result<usize> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let resp = http
        .post(endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(auth_header_name, auth_header_value)
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("upstream tools/list returned HTTP {}", resp.status());
    }

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
    let result = json
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("tools/list response missing `result` field"))?
        .clone();
    let parsed: McpToolsListResult = serde_json::from_value(result)?;

    // Replace this user's tool set atomically: clear + reinsert in one
    // tx so concurrent reads never observe an empty list. Cheaper than
    // a deactivate/upsert/cleanup since `mcp_user_tools` doesn't need
    // an `is_active` column — gone means gone.
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM mcp_user_tools WHERE mcp_server_id = $1 AND user_id = $2")
        .bind(server_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if !parsed.tools.is_empty() {
        let names: Vec<&str> = parsed.tools.iter().map(|t| t.name.as_str()).collect();
        let descriptions: Vec<Option<String>> =
            parsed.tools.iter().map(|t| t.description.clone()).collect();
        let input_schemas: Vec<serde_json::Value> = parsed
            .tools
            .iter()
            .map(|t| t.input_schema.clone().unwrap_or(serde_json::Value::Null))
            .collect();
        sqlx::query(
            r#"INSERT INTO mcp_user_tools
                  (mcp_server_id, user_id, tool_name, description, input_schema, discovered_at)
               SELECT $1, $2, name, descr, schema, now()
                 FROM UNNEST($3::text[], $4::text[], $5::jsonb[]) AS t(name, descr, schema)
               ON CONFLICT (mcp_server_id, user_id, tool_name)
               DO UPDATE SET description = EXCLUDED.description,
                             input_schema = EXCLUDED.input_schema,
                             discovered_at = now()"#,
        )
        .bind(server_id)
        .bind(user_id)
        .bind(&names)
        .bind(&descriptions)
        .bind(&input_schemas)
        .execute(&mut *tx)
        .await?;
    }
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
                SystemDiscoveryOutcome::Tools(n) => {
                    tracing::info!(
                        mcp_server = %server.name,
                        tools = n,
                        "MCP tool discovery refreshed"
                    );
                    if let Ok(updated) = build_registered_server(&db, &server, &key).await {
                        registry.register(updated).await;
                    }
                }
                SystemDiscoveryOutcome::AuthRequired => {
                    // Server needs per-user auth — system-level
                    // discovery is *expected* to be empty here. Don't
                    // log warn (would alarm admins for the steady-state).
                    tracing::debug!(
                        mcp_server = %server.name,
                        "anon tools/list rejected; tools populate per user on first call"
                    );
                }
                SystemDiscoveryOutcome::Failed(e) => {
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
    // Wrap in supervise_restart so a single panic in the probe path
    // (registry list, SSE parse, DB query) doesn't permanently kill
    // health monitoring. Without this, the admin UI silently freezes
    // showing whatever state the registry had at the moment of the
    // panic.
    think_watch_common::tasks::supervise_restart("mcp_health_loop", move || {
        let state = state.clone();
        let registry = registry.clone();
        let checker = checker.clone();
        async move {
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
                        think_watch_mcp_gateway::registry::ServerStatus::Disconnected => {
                            "disconnected"
                        }
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
        }
    });
}

/// Background catalog-refresh loop. Re-runs `discover_and_persist_tools`
/// against every connected MCP server once every 24 hours so the
/// `cached_tools_jsonb` snapshot — which is what users that haven't
/// authorized yet see in `tools/list` — doesn't drift from the
/// upstream's actual catalog over time.
///
/// Anonymous probe (no per-user token) — for upstreams that gate
/// `tools/list` behind authentication this naturally fails and the
/// existing snapshot is preserved.
pub fn spawn_mcp_catalog_refresh_loop(
    state: AppState,
    registry: think_watch_mcp_gateway::registry::Registry,
) {
    const CATALOG_REFRESH_INTERVAL_SECS: u64 = 24 * 60 * 60;
    // Wrap in supervise_restart for the same reason as the health
    // loop — a panic in `discover_and_persist_tools` (SSE parse, JSON
    // decode, sqlx) would otherwise permanently freeze the cached
    // tool catalog and silently let it drift from upstream reality.
    think_watch_common::tasks::supervise_restart("mcp_catalog_refresh_loop", move || {
        let state = state.clone();
        let registry = registry.clone();
        async move {
            // Stagger the first run so it doesn't pile on top of the
            // startup discovery burst initiated by `load_mcp_servers_into_registry`.
            tokio::time::sleep(std::time::Duration::from_secs(
                CATALOG_REFRESH_INTERVAL_SECS,
            ))
            .await;
            loop {
                let servers = match sqlx::query_as::<_, think_watch_common::models::McpServer>(
                    "SELECT * FROM mcp_servers",
                )
                .fetch_all(&state.db)
                .await
                {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::warn!("catalog refresh: failed to list servers: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(
                            CATALOG_REFRESH_INTERVAL_SECS,
                        ))
                        .await;
                        continue;
                    }
                };
                let key = state.config.encryption_key.clone();
                for server in &servers {
                    let http = (**state.http_client.load()).clone();
                    match discover_and_persist_tools(&state.db, &http, server).await {
                        SystemDiscoveryOutcome::Tools(n) => {
                            tracing::info!(
                                mcp_server = %server.name,
                                tools = n,
                                "MCP catalog refresh succeeded"
                            );
                            if let Ok(updated) =
                                build_registered_server(&state.db, server, &key).await
                            {
                                registry.register(updated).await;
                            }
                        }
                        SystemDiscoveryOutcome::AuthRequired => {
                            // Steady state for OAuth/static-token servers.
                            // Per-user discovery happens on first call.
                        }
                        SystemDiscoveryOutcome::Failed(e) => {
                            tracing::debug!(
                                mcp_server = %server.name,
                                error = %e,
                                "MCP catalog refresh failed (cached snapshot retained)"
                            );
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(
                    CATALOG_REFRESH_INTERVAL_SECS,
                ))
                .await;
            }
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
