use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use think_watch_common::limits::{
    self, RateLimitRule, RateLimitSubject, RateMetric, Surface, SurfaceConstraints, sliding,
};

use crate::access_control::is_tool_allowed;
use crate::cache::{CallerScope, McpResponseCache};
use crate::circuit_breaker::McpCircuitBreakers;
use crate::pool::ConnectionPool;
use crate::registry::{Registry, ServerCacheScope};
use crate::session::SessionManager;
use crate::user_token::{
    AuthInjection, CredentialOwner, RefreshFailureKind, ResolverCaller, ResolverError,
    UserTokenResolver,
};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// Standard JSON-RPC error codes.
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
/// ThinkWatch-specific: the calling user (or the calling API key's
/// chosen account label) has no upstream credential for the requested
/// MCP server. The `data` field carries `server_id` and an optional
/// `authorize_url`. Outside the JSON-RPC reserved range (-32768 …
/// -32000) so it can't collide with anything the spec assigns later.
pub const NEEDS_USER_CREDENTIALS: i32 = -32050;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_response(id: Option<serde_json::Value>, result: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result: Some(result),
        error: None,
    }
}

pub fn err_response(
    id: Option<serde_json::Value>,
    code: i32,
    message: impl Into<String>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Body-capture truncation + offload + PII redaction hook for
/// `mcp_logs.tool_arguments` / `tool_result`. Mirrors the gateway-side
/// `process_body` so both audit pipelines share char-boundary-safe
/// truncation + the same blob-offload contract; we don't share the
/// helper across crates because the gateway's PiiRedactor isn't in
/// `common` and pulling it down would invert the dep graph.
///
/// `redact_pii` is currently a no-op for mcp (no PII engine yet).
/// When enabled the body is left raw and an operator sees the toggle's
/// intent reflected via the `audit.body_redact_pii` setting but the
/// redaction itself is a follow-up.
#[allow(clippy::too_many_arguments)]
async fn apply_mcp_body_capture(
    mut s: String,
    max_bytes: usize,
    _redact_pii: bool,
    blob_store: &std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    trace_id: &str,
    field: &'static str,
    truncated_flag: &mut bool,
    offloaded_flag: &mut bool,
) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    if blob_store.can_offload() {
        use think_watch_common::blob_store::{BlobDecision, BlobKeyHint};
        let hint = BlobKeyHint {
            table: "mcp_logs",
            log_id: trace_id,
            field,
        };
        match blob_store
            .store_if_oversize(hint, std::mem::take(&mut s), max_bytes)
            .await
        {
            Ok(BlobDecision::Offloaded { url, .. }) => {
                *offloaded_flag = true;
                return url;
            }
            Ok(BlobDecision::Inline(returned)) => {
                s = returned;
            }
            Err(e) => {
                tracing::warn!(
                    field,
                    trace_id,
                    error = %e,
                    "MCP blob offload failed; falling back to truncation"
                );
                metrics::counter!(
                    "audit_body_offload_failed_total",
                    "field" => field.to_string()
                )
                .increment(1);
                s = format!("[blob offload failed: {e}]");
            }
        }
    }
    *truncated_flag = true;
    let budget = max_bytes.saturating_sub(3);
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}

/// Build a human label for a rate-limit rule. Same shape the AI
/// gateway uses (`subject:metric/window`) so log scrapers see one
/// consistent format across surfaces.
fn rate_label(rule: &limits::RateLimitRule) -> String {
    let window = match rule.window_secs {
        60 => "1m".to_string(),
        300 => "5m".to_string(),
        3_600 => "1h".to_string(),
        18_000 => "5h".to_string(),
        86_400 => "1d".to_string(),
        604_800 => "1w".to_string(),
        n => format!("{n}s"),
    };
    format!(
        "{}:{}/{}",
        rule.subject_kind.as_str(),
        rule.metric.as_str(),
        window
    )
}

// ---------------------------------------------------------------------------
// Per-request caller context
// ---------------------------------------------------------------------------

/// Everything the proxy needs to know about the caller for a single
/// request dispatch.  Bundled into a struct to keep method signatures
/// under the clippy `too_many_arguments` threshold.
pub struct RequestContext<'a> {
    pub user_id: Uuid,
    pub user_email: &'a str,
    pub client_session_id: &'a str,
    pub surface_constraints: &'a SurfaceConstraints,
    pub allowed_mcp_tools: Option<&'a [String]>,
    pub trace_id: &'a str,
    /// Per-server MCP account override JSON from the calling API key
    /// — `{ "<server_uuid>": "<account_label>" }`. Empty `{}` ⇒ the
    /// resolver always picks the user's `is_default` credential.
    pub mcp_account_overrides: &'a serde_json::Value,
    /// Resolved client IP, snapshotted onto every `mcp_logs` row.
    pub ip_address: Option<&'a str>,
}

/// Read the account_label routed to a specific server from the API
/// key's `mcp_account_overrides` map, without going through the full
/// credential resolver.
///
/// The full resolver does work the cache layer doesn't need (decrypt
/// secret, refresh token, write back). For cache keying we only care
/// about the *label string* the caller said to use — that's what
/// makes "personal vs work" GitHub responses land in distinct cache
/// entries. When the caller didn't pass an override (`None`), we
/// scope to user_id only and accept up to one TTL window of staleness
/// if the user later flips their default credential.
fn account_label_for_server(overrides: &serde_json::Value, server_id: Uuid) -> Option<&str> {
    overrides
        .get(server_id.to_string())
        .and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// McpProxy
// ---------------------------------------------------------------------------

/// The core MCP proxy.  Receives JSON-RPC requests from clients, aggregates
/// tool lists from all registered upstream servers, enforces access control,
/// and forwards tool calls to the correct upstream server.
///
/// Holds the same `db` / `redis` handles the AI gateway uses so the
/// shared `limits` engine can be queried per request without bouncing
/// out to a separate service. Quotas live in `rate_limit_rules` and
/// are configurable per user / per MCP server.
#[derive(Clone)]
pub struct McpProxy {
    pub registry: Registry,
    pub pool: ConnectionPool,
    pub circuit_breakers: McpCircuitBreakers,
    /// Per-user session manager — owns the mapping from client session
    /// to per-server upstream `Mcp-Session-Id` values.  This is the
    /// single source of truth for upstream sessions, replacing the
    /// previous per-connection state that was shared across users.
    pub sessions: SessionManager,
    /// Redis-backed response cache for MCP tool calls. The cache lane
    /// is determined per-server via [`ServerCacheScope`]: `Global`
    /// servers share one entry across users; `PerCaller` servers
    /// scope by `(user_id, account_label?)` so OAuth/PAT responses
    /// can't leak across callers.
    pub cache: McpResponseCache,
    pub db: PgPool,
    pub redis: fred::clients::Client,
    pub dynamic_config: std::sync::Arc<think_watch_common::dynamic_config::DynamicConfig>,
    /// Audit sink — populated by the server when it constructs the
    /// proxy. One `mcp_logs` row per tools/call completion, tagged
    /// with trace_id so the /api/admin/trace view can correlate it
    /// with the AI-gateway row that triggered the call.
    pub audit: think_watch_common::audit::AuditLogger,
    /// Per-user upstream credential resolver — picks (and refreshes)
    /// the OAuth / static-token credential for the calling user-account
    /// pair before each `tools/call` reaches the upstream.
    pub user_tokens: UserTokenResolver,
    /// Body-offload store. Oversize tool arguments / results land here
    /// instead of inline `mcp_logs.tool_arguments` / `tool_result`
    /// columns. Same backing store the AI gateway uses.
    pub blob_store: std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
}

impl McpProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Registry,
        pool: ConnectionPool,
        sessions: SessionManager,
        db: PgPool,
        redis: fred::clients::Client,
        dynamic_config: std::sync::Arc<think_watch_common::dynamic_config::DynamicConfig>,
        audit: think_watch_common::audit::AuditLogger,
        user_tokens: UserTokenResolver,
        blob_store: std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    ) -> Self {
        let cache = McpResponseCache::new(redis.clone());
        Self {
            registry,
            pool,
            circuit_breakers: McpCircuitBreakers::new(),
            sessions,
            cache,
            db,
            redis,
            dynamic_config,
            audit,
            user_tokens,
            blob_store,
        }
    }

    /// Main entry point: dispatch a single JSON-RPC request from a client.
    /// `user_roles` is required because the access controller is now
    /// default-deny — without role information non-admin users would be
    /// rejected even when an explicit per-tool policy permits them.
    pub async fn handle_request(
        &self,
        ctx: &RequestContext<'_>,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => self.handle_initialize(request).await,
            "tools/list" => self.handle_tools_list(ctx, request).await,
            "tools/call" => self.handle_tools_call(ctx, request).await,
            _ => err_response(
                request.id,
                METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
            ),
        }
    }

    // -----------------------------------------------------------------------
    // initialize
    // -----------------------------------------------------------------------

    async fn handle_initialize(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let capabilities = serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": {},
                "prompts": {}
            },
            "serverInfo": {
                "name": "ThinkWatch MCP Gateway",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        ok_response(request.id, capabilities)
    }

    // -----------------------------------------------------------------------
    // tools/list
    // -----------------------------------------------------------------------

    async fn handle_tools_list(
        &self,
        ctx: &RequestContext<'_>,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        // Pre-compute the set of server_ids this user already has a
        // credential for so the per-tool annotation step doesn't run
        // a SELECT per registered tool.
        let connected_server_ids: std::collections::HashSet<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT mcp_server_id FROM mcp_user_credentials WHERE user_id = $1",
        )
        .bind(ctx.user_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

        // Pre-load this user's per-server tool catalogs in one query —
        // for auth-required servers (OAuth / static-token / template
        // headers), tools/list responses can differ per user (Atlassian-
        // style scope filtering), so server-level `mcp_tools` rows are
        // intentionally empty there. The per-user view lives in
        // `mcp_user_tools`, populated either by the credential-write
        // hook (oauth_callback / paste_static_token) or by lazy
        // discovery on first proxy call (see below).
        #[derive(sqlx::FromRow)]
        struct UserToolRow {
            mcp_server_id: Uuid,
            tool_name: String,
            description: Option<String>,
            input_schema: Option<serde_json::Value>,
        }
        let user_tool_rows: Vec<UserToolRow> = sqlx::query_as(
            "SELECT mcp_server_id, tool_name, description, input_schema
               FROM mcp_user_tools WHERE user_id = $1",
        )
        .bind(ctx.user_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        let mut user_tools_by_server: std::collections::HashMap<Uuid, Vec<UserToolRow>> =
            std::collections::HashMap::new();
        for row in user_tool_rows {
            user_tools_by_server
                .entry(row.mcp_server_id)
                .or_default()
                .push(row);
        }

        let servers = self.registry.list().await;
        let mut tools: Vec<serde_json::Value> = Vec::new();
        for server in servers {
            // Admin-shared servers: there's exactly one upstream
            // identity, so the catalog is identical for every caller
            // and lives in `server.tools` (populated by
            // `spawn_shared_tool_discovery` on credential write).
            // Skip the per-user pathway entirely.
            let admin_shared = server.credential_owner == CredentialOwner::AdminShared;
            let server_needs_auth =
                !matches!(server.auth_shape, crate::user_token::AuthShape::Anonymous,);
            let user_connected = connected_server_ids.contains(&server.id);
            // For admin_shared the credential is provisioned by an
            // admin, not by the calling user — it never makes sense
            // to mark requires_user_auth on these.
            let requires_user_auth = !admin_shared && server_needs_auth && !user_connected;

            // Pick the tool source for this server:
            //
            // - **Admin-shared / direct-mode server**: `server.tools`
            //   carries the catalog and is identical for every caller.
            // - **Per-user auth-required + user connected**: the user's
            //   own filtered catalog from `mcp_user_tools`. Falls back
            //   to `server.tools` (typically empty) if the eager hook
            //   missed; lazy refresh kicked off in the background.
            // - **Per-user auth-required + user NOT connected**: emit
            //   `_meta.requires_user_auth = true` with whatever
            //   metadata the system-level catalog has.
            enum ToolSource<'a> {
                System(&'a [crate::registry::McpToolInfo]),
                User(&'a [UserToolRow]),
            }
            let user_rows = user_tools_by_server.get(&server.id);
            let source = if admin_shared {
                ToolSource::System(server.tools.as_slice())
            } else if server_needs_auth && user_connected {
                match user_rows.filter(|v| !v.is_empty()) {
                    Some(rows) => ToolSource::User(rows.as_slice()),
                    None => {
                        self.spawn_lazy_user_tool_discovery(&server, ctx);
                        ToolSource::System(server.tools.as_slice())
                    }
                }
            } else {
                ToolSource::System(server.tools.as_slice())
            };

            // Yields (name, description, schema) tuples for whichever
            // source we picked, so the iteration loop is one-form.
            let source_iter: Box<
                dyn Iterator<Item = (&str, Option<&str>, Option<&serde_json::Value>)>,
            > = match source {
                ToolSource::System(t) => Box::new(t.iter().map(|t| {
                    (
                        t.name.as_str(),
                        t.description.as_deref(),
                        t.input_schema.as_ref(),
                    )
                })),
                ToolSource::User(t) => Box::new(t.iter().map(|t| {
                    (
                        t.tool_name.as_str(),
                        t.description.as_deref(),
                        t.input_schema.as_ref(),
                    )
                })),
            };

            for (tool_name, tool_desc, tool_schema) in source_iter {
                let namespaced = format!(
                    "{}{}{}",
                    server.namespace_prefix,
                    crate::registry::NAMESPACE_SEPARATOR,
                    tool_name,
                );
                if !is_tool_allowed(ctx.allowed_mcp_tools, &namespaced) {
                    continue;
                }
                let mut entry = serde_json::json!({
                    "name": namespaced,
                    "description": tool_desc.unwrap_or_default(),
                    "inputSchema": tool_schema
                        .cloned()
                        .unwrap_or(serde_json::json!({"type": "object"})),
                });
                if requires_user_auth && let Some(obj) = entry.as_object_mut() {
                    obj.insert(
                        "_meta".to_string(),
                        serde_json::json!({
                            "requires_user_auth": true,
                            "server_id": server.id.to_string(),
                            "server_name": server.name,
                        }),
                    );
                }
                tools.push(entry);
            }
        }

        ok_response(request.id, serde_json::json!({ "tools": tools }))
    }

    /// Fire-and-forget lazy discovery for a (user, server) pair whose
    /// `mcp_user_tools` cache is missing. Uses the same upstream-call
    /// machinery as `tools/call` — `UserTokenResolver` to grab the
    /// bearer, `ConnectionPool::send_request` to issue tools/list, then
    /// writes the parsed list to `mcp_user_tools`. The current request
    /// returns immediately with whatever was cached (typically empty);
    /// the user's next tools/list call sees the fresh catalog.
    fn spawn_lazy_user_tool_discovery(
        &self,
        server: &crate::registry::RegisteredServer,
        ctx: &RequestContext<'_>,
    ) {
        // Admin-shared servers have a single tool catalog populated on
        // shared-credential write; per-user discovery is not just
        // wasteful, it'd write `mcp_user_tools` rows that the
        // `mcp_tools` query short-circuits past anyway.
        if server.credential_owner == CredentialOwner::AdminShared {
            return;
        }
        let server = server.clone();
        let user_id = ctx.user_id;
        let resolver_caller = ResolverCaller {
            user_id,
            mcp_account_overrides: ctx.mcp_account_overrides.clone(),
        };
        let user_tokens = self.user_tokens.clone();
        let pool = self.pool.clone();
        let db = self.db.clone();
        let server_auth_cfg = server.auth_cfg();
        tokio::spawn(async move {
            let auth_header = match user_tokens
                .resolve(server.id, &server_auth_cfg, &resolver_caller)
                .await
            {
                Ok(Some(h)) => h,
                Ok(None) => return,
                Err(e) => {
                    tracing::debug!(
                        server = %server.name,
                        user_id = %user_id,
                        error = ?e,
                        "lazy user-tool discovery skipped: resolver could not produce a credential"
                    );
                    return;
                }
            };

            let conn = pool.get_or_create(&server).await;
            let req = JsonRpcRequest {
                jsonrpc: "2.0".to_owned(),
                id: Some(serde_json::json!("user-tools-lazy-discover")),
                method: "tools/list".to_owned(),
                params: None,
            };
            let resp = match pool
                .send_request(&conn, &req, Some(auth_header.as_pair()), None, None, None)
                .await
            {
                Ok((r, _)) => r,
                Err(e) => {
                    tracing::warn!(
                        server = %server.name,
                        user_id = %user_id,
                        error = %e,
                        "lazy user-tool discovery: upstream tools/list failed"
                    );
                    return;
                }
            };
            let Some(result) = resp.result else { return };
            let Some(tools) = result.get("tools").and_then(|v| v.as_array()) else {
                return;
            };

            let mut tx = match db.begin().await {
                Ok(t) => t,
                Err(_) => return,
            };
            if sqlx::query("DELETE FROM mcp_user_tools WHERE mcp_server_id = $1 AND user_id = $2")
                .bind(server.id)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .is_err()
            {
                return;
            }
            // Batch upsert via UNNEST — same pattern as the eager
            // path in mcp_runtime.rs. A 50-tool list used to fire
            // 50 sequential round-trips inside the lazy-discover TX.
            let mut names: Vec<&str> = Vec::with_capacity(tools.len());
            let mut descs: Vec<Option<&str>> = Vec::with_capacity(tools.len());
            let mut schemas: Vec<serde_json::Value> = Vec::with_capacity(tools.len());
            for t in tools {
                let Some(name) = t.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                names.push(name);
                descs.push(t.get("description").and_then(|v| v.as_str()));
                schemas.push(
                    t.get("inputSchema")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            if !names.is_empty() {
                let descs_owned: Vec<Option<String>> =
                    descs.iter().map(|s| s.map(String::from)).collect();
                let _ = sqlx::query(
                    r#"INSERT INTO mcp_user_tools
                          (mcp_server_id, user_id, tool_name, description, input_schema, discovered_at)
                       SELECT $1, $2, name, descr, schema, now()
                         FROM UNNEST($3::text[], $4::text[], $5::jsonb[]) AS t(name, descr, schema)
                       ON CONFLICT (mcp_server_id, user_id, tool_name)
                       DO UPDATE SET description = EXCLUDED.description,
                                     input_schema = EXCLUDED.input_schema,
                                     discovered_at = now()"#,
                )
                .bind(server.id)
                .bind(user_id)
                .bind(&names)
                .bind(&descs_owned)
                .bind(&schemas)
                .execute(&mut *tx)
                .await;
            }
            let _ = tx.commit().await;
            tracing::info!(
                server = %server.name,
                user_id = %user_id,
                tools = tools.len(),
                "lazy user-tool discovery completed"
            );
        });
    }

    // -----------------------------------------------------------------------
    // tools/call
    // -----------------------------------------------------------------------

    async fn handle_tools_call(
        &self,
        ctx: &RequestContext<'_>,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let user_id = ctx.user_id;
        let user_email = ctx.user_email;
        let client_session_id = ctx.client_session_id;
        let surface_constraints = ctx.surface_constraints;
        let allowed_mcp_tools = ctx.allowed_mcp_tools;
        let trace_id = ctx.trace_id;
        // Resolve params + tool target up front so we know which MCP
        // server this call belongs to. The rate-limit subjects need
        // the server id, so the rate-limit gate runs AFTER the
        // server lookup but BEFORE access control + the real call.

        let params = match &request.params {
            Some(p) => p,
            None => {
                return err_response(request.id, INVALID_PARAMS, "Missing params for tools/call");
            }
        };

        let namespaced_name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return err_response(
                    request.id,
                    INVALID_PARAMS,
                    "Missing or invalid 'name' in params",
                );
            }
        };

        // Resolve server + original tool name from the namespaced identifier.
        let (server, original_tool_name) =
            match self.registry.find_server_for_tool(namespaced_name).await {
                Some(pair) => pair,
                None => {
                    return err_response(
                        request.id,
                        INVALID_PARAMS,
                        format!("Unknown tool: {namespaced_name}"),
                    );
                }
            };

        // Rate-limit pre-flight — materialize the user's merged MCP
        // surface rules on the fly (the parent crate already did the
        // most-restrictive aggregation across every role assignment).
        let rules: Vec<RateLimitRule> = surface_constraints
            .block(Surface::McpGateway)
            .map(|block| {
                block
                    .rules
                    .iter()
                    .filter(|r| r.enabled)
                    .map(|r| RateLimitRule {
                        id: Uuid::nil(),
                        subject_kind: RateLimitSubject::User,
                        subject_id: user_id,
                        surface: Surface::McpGateway,
                        metric: r.metric,
                        window_secs: r.window_secs,
                        max_count: r.max_count,
                        enabled: true,
                        expires_at: None,
                        reason: None,
                        created_by: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let resolved = sliding::resolve_rules(&rules, RateMetric::Requests);
        if !resolved.is_empty() {
            let fail_closed = self.dynamic_config.rate_limit_fail_closed().await;
            let outcome =
                match sliding::check_and_record(&self.redis, &resolved, 1, !fail_closed).await {
                    Ok(o) => o,
                    Err(e) => {
                        if fail_closed {
                            tracing::warn!("MCP rate-limit redis error: {e}; failing closed");
                            return err_response(
                                request.id,
                                INVALID_REQUEST,
                                "Rate limited: rate_limiter_unavailable".to_string(),
                            );
                        }
                        tracing::warn!("MCP rate-limit redis error: {e}; allowing call");
                        sliding::CheckOutcome {
                            allowed: true,
                            exceeded_index: -1,
                            currents: Vec::new(),
                        }
                    }
                };
            if !outcome.allowed {
                let label = (outcome.exceeded_index >= 0)
                    .then(|| {
                        rules
                            .iter()
                            .filter(|r| r.metric == RateMetric::Requests)
                            .nth(outcome.exceeded_index as usize)
                            .map(rate_label)
                    })
                    .flatten()
                    .unwrap_or_else(|| "rate limit".to_string());
                tracing::warn!(user_id = %user_id, server = %server.name, "MCP rate limited: {label}");
                metrics::counter!("mcp_rate_limited_total").increment(1);
                return err_response(
                    request.id,
                    INVALID_REQUEST,
                    format!("Rate limited: {label}"),
                );
            }
        }

        // Access control: check tool against the user's allowed_mcp_tools patterns.
        if !is_tool_allowed(allowed_mcp_tools, namespaced_name) {
            return err_response(request.id, INVALID_REQUEST, "Access denied for this tool");
        }

        // Build the upstream request with the original (un-namespaced) tool
        // name.
        let mut upstream_params = params.clone();
        if let Some(obj) = upstream_params.as_object_mut() {
            obj.insert(
                "name".to_owned(),
                serde_json::Value::String(original_tool_name.clone()),
            );
        }

        let upstream_request = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: request.id.clone(),
            method: "tools/call".to_owned(),
            params: Some(upstream_params),
        };

        // --- Response cache ---------------------------------------------------
        // Resolve the effective cache TTL:
        //   per-server override (0 = explicitly disabled) → global fallback
        let effective_cache_ttl = server
            .cache_ttl_secs
            .unwrap_or(self.dynamic_config.mcp_cache_ttl_secs().await);

        // Build the per-request cache scope from the server's static
        // `ServerCacheScope` plus, for PerCaller servers, the
        // user_id and the optional account_label routed by the
        // calling API key. PerCaller without an account override
        // collapses to per-user — the user's *default* credential is
        // implicit; if they switch defaults they get at most TTL
        // seconds of stale cache, which is acceptable.
        let cache_scope = match server.cache_scope {
            ServerCacheScope::Global => None,
            ServerCacheScope::PerCaller => Some(CallerScope {
                user_id: &user_id,
                account_label: account_label_for_server(ctx.mcp_account_overrides, server.id),
            }),
        };

        if effective_cache_ttl > 0 {
            if let Some(cached) = self
                .cache
                .get(&server.id, cache_scope, &upstream_request)
                .await
            {
                metrics::counter!("mcp_cache_hits_total").increment(1);
                tracing::debug!(server = %server.name, "MCP cache hit");
                return cached;
            }
            metrics::counter!("mcp_cache_misses_total").increment(1);
        }

        // Circuit breaker — fail fast if the server's CB is currently Open.
        // The breaker is keyed by server name, which is what the dashboard
        // upstream-health panel reads from the shared cb_registry.
        if self.circuit_breakers.check(&server.name).await.is_err() {
            tracing::warn!(
                server = %server.name,
                "tools/call short-circuited: MCP circuit breaker open"
            );
            return err_response(
                request.id,
                INTERNAL_ERROR,
                format!(
                    "Upstream MCP server '{}' is temporarily unavailable",
                    server.name
                ),
            );
        }

        // Get (or create) a connection and forward the request.
        let conn = self.pool.get_or_create(&server).await;
        let caller = crate::pool::CallerIdentity {
            user_id: user_id.to_string(),
            user_email: user_email.to_string(),
        };

        // Retrieve the per-user upstream session ID for this server
        // from the SessionManager (backed by Redis in production).
        let upstream_sid = self
            .sessions
            .get_upstream_session(client_session_id, server.id)
            .await;

        // The trace id was resolved at the transport layer — either
        // pinned by an upstream `x-trace-id` header so this call links
        // to the AI request that triggered the tool-use, or freshly
        // minted if the caller didn't supply one.
        let call_trace_id = trace_id.to_string();
        let started = std::time::Instant::now();
        let server_id = server.id;
        let server_name = server.name.clone();
        let tool_name = original_tool_name.to_string();

        // Resolve the per-user upstream credential. Errors here mean
        // the user (or this API key's chosen account label) hasn't
        // connected yet — surface NEEDS_USER_CREDENTIALS so the console
        // can guide them to /connections instead of 500-ing.
        let resolver_caller = ResolverCaller {
            user_id,
            mcp_account_overrides: ctx.mcp_account_overrides.clone(),
        };
        let server_auth_cfg = server.auth_cfg();
        let auth_header = match self
            .user_tokens
            .resolve(server_id, &server_auth_cfg, &resolver_caller)
            .await
        {
            Ok(opt) => opt,
            Err(ResolverError::NeedsUserCredentials {
                server_id,
                authorize_url,
                owner,
            }) => {
                // Hydrate authorize_url from the registered server when
                // the resolver couldn't supply one (it doesn't see the
                // server's OAuth config). With this, AI agents can
                // surface a clickable re-auth link instead of asking
                // the user to navigate to the console manually.
                let resolved_authorize_url = authorize_url.or_else(|| {
                    server
                        .oauth_cfg
                        .as_ref()
                        .and_then(|c| c.authorization_endpoint.clone())
                });
                let (msg, console_url) = match owner {
                    CredentialOwner::PerUser => (
                        format!(
                            "User has not connected an account for MCP server '{server_name}'. \
                             Open /connections in the ThinkWatch console to authorize."
                        ),
                        "/connections",
                    ),
                    CredentialOwner::AdminShared => (
                        format!(
                            "MCP server '{server_name}' is configured to use a shared \
                             credential, but no admin has provisioned one yet. \
                             Ask an administrator to configure it in the server settings."
                        ),
                        "/admin/mcp/servers",
                    ),
                };
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_owned(),
                    id: request.id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: NEEDS_USER_CREDENTIALS,
                        message: msg,
                        data: Some(serde_json::json!({
                            "kind": "needs_user_credentials",
                            "server_id": server_id.to_string(),
                            "server_name": server_name,
                            "authorize_url": resolved_authorize_url,
                            "console_url": console_url,
                            "owner": owner.as_str(),
                        })),
                    }),
                };
            }
            Err(ResolverError::RefreshFailed {
                server_id,
                kind: RefreshFailureKind::Permanent,
                ..
            }) => {
                // Credential gone — refresh was rejected. Tell whoever
                // is responsible (caller for per_user, admin for
                // admin_shared) to re-authorize.
                let console_url = match server.credential_owner {
                    CredentialOwner::PerUser => "/connections",
                    CredentialOwner::AdminShared => "/admin/mcp/servers",
                };
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_owned(),
                    id: request.id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: NEEDS_USER_CREDENTIALS,
                        message: format!(
                            "Authorization for MCP server '{server_name}' was rejected by the \
                             upstream and has been cleared. Re-authorize at {console_url}."
                        ),
                        data: Some(serde_json::json!({
                            "kind": "needs_user_credentials",
                            "reason": "refresh_rejected",
                            "server_id": server_id.to_string(),
                            "server_name": server_name,
                            "authorize_url": server
                                .oauth_cfg
                                .as_ref()
                                .and_then(|c| c.authorization_endpoint.clone()),
                            "console_url": console_url,
                            "owner": server.credential_owner.as_str(),
                        })),
                    }),
                };
            }
            Err(ResolverError::RefreshFailed {
                kind: RefreshFailureKind::Transient,
                message,
                ..
            }) => {
                return err_response(
                    request.id.clone(),
                    INTERNAL_ERROR,
                    format!(
                        "Upstream OAuth provider for MCP server '{server_name}' is \
                         temporarily unavailable ({message}). Retry in a few seconds."
                    ),
                );
            }
            Err(e) => {
                tracing::error!(
                    server_id = %server_id,
                    error = %e,
                    "credential resolver failed"
                );
                return err_response(
                    request.id.clone(),
                    INTERNAL_ERROR,
                    format!("Credential resolver failed: {e}"),
                );
            }
        };
        let auth_ref = auth_header.as_ref().map(AuthInjection::as_pair);

        let response = match self
            .pool
            .send_request(
                &conn,
                &upstream_request,
                auth_ref,
                Some(&caller),
                upstream_sid.as_deref(),
                Some(&call_trace_id),
            )
            .await
        {
            Ok((resp, new_upstream_sid)) => {
                // Persist any upstream session ID the server returned so
                // subsequent calls from this user reuse the same session.
                if let Some(sid) = new_upstream_sid {
                    self.sessions
                        .set_upstream_session(client_session_id, server_id, sid)
                        .await;
                }

                // JSON-RPC error responses count toward the breaker
                // ONLY when the upstream returned a server-side failure
                // code. The previous "any error trips the breaker"
                // rule punished every user on a shared server for one
                // user's bad input — e.g. five INVALID_PARAMS or
                // METHOD_NOT_FOUND replies from a single misbehaving
                // client opened the breaker and denied every other
                // user for the full cooldown.
                //
                // JSON-RPC 2.0 error code ranges (server-side):
                //   -32603             — Internal error
                //   -32000 .. -32099   — Implementation-defined server errors
                // Everything else (-32600 invalid request, -32601 method
                // not found, -32602 invalid params, -32700 parse error,
                // and our own custom application codes like
                // NEEDS_USER_CREDENTIALS = -32050) is caller-attributable
                // and must NOT count toward the breaker.
                //
                // The `record_cb_with_kind` call inside the breaker fires
                // the global OPEN_LISTENER installed by the server, which
                // emits `provider.circuit_open` audit events uniformly
                // for AI and MCP backends — no per-call emission here.
                let is_server_failure = resp
                    .error
                    .as_ref()
                    .map(|err| {
                        let c = err.code;
                        c == INTERNAL_ERROR || (-32099..=-32000).contains(&c)
                    })
                    .unwrap_or(false);
                if is_server_failure {
                    self.circuit_breakers.record_failure(&server_name).await;
                } else {
                    // Success OR caller-side error — both indicate the
                    // upstream is reachable and responsive, so credit
                    // the half-open probe / reset the failure counter.
                    self.circuit_breakers.record_success(&server_name).await;
                }
                resp
            }
            Err(e) => {
                self.circuit_breakers.record_failure(&server_name).await;
                tracing::error!(
                    server_id = %server_id,
                    error = %e,
                    "upstream tools/call failed"
                );
                err_response(
                    request.id.clone(),
                    INTERNAL_ERROR,
                    format!("Upstream server error: {e}"),
                )
            }
        };

        // Write successful responses to cache when caching is enabled.
        if effective_cache_ttl > 0 && response.error.is_none() {
            self.cache
                .set(
                    &server_id,
                    cache_scope,
                    &upstream_request,
                    &response,
                    effective_cache_ttl,
                )
                .await;
        }

        // Emit mcp_logs row so /api/admin/trace lights up this call.
        // Tool discovery (`tools/list`) is deliberately excluded here —
        // we're inside `handle_tools_call` already, so `tool_name` is
        // always an actual invocation.
        let (status, error_message) = if let Some(ref err) = response.error {
            ("error".to_string(), Some(err.message.clone()))
        } else {
            ("ok".to_string(), None)
        };
        // Capture the call arguments alongside the tool name so the
        // trace endpoint can show what was actually invoked. Secret-
        // shaped keys are redacted by sanitize_detail downstream
        // (recursive walk over the JSON tree, see common::audit).
        let logged_arguments = params.get("arguments").cloned();
        use think_watch_common::audit::{AuditActor, McpActor};
        let actor = McpActor {
            user_id,
            user_email,
            ip: ctx.ip_address,
        };

        // Body capture for audit. arguments + upstream result land in
        // dedicated `mcp_logs.tool_arguments` / `mcp_logs.tool_result`
        // columns (separate from the metadata-only `detail` JSON) so
        // auditors can query them without parsing JSON per row. Gated
        // by `audit.capture_tool_arguments` / `audit.capture_tool_results`
        // with the same `audit.body_max_bytes` truncation contract
        // the gateway side uses; defaults ON (the bastion positioning
        // requires it). PII redaction follows `audit.body_redact_pii`
        // — applied at write time, not in-flight.
        let dc = &self.dynamic_config;
        let capture_args = dc.audit_capture_tool_arguments().await;
        let capture_result = dc.audit_capture_tool_results().await;
        let body_max = dc.audit_body_max_bytes().await as usize;
        let redact = dc.audit_body_redact_pii().await;
        let (arg_str, result_str, capture_status) = if !capture_args && !capture_result {
            (None, None, Some("disabled".to_owned()))
        } else {
            let mut truncated = false;
            let mut offloaded = false;
            let arg_str = if capture_args {
                match logged_arguments.as_ref() {
                    Some(v) => {
                        let raw = serde_json::to_string(v)
                            .unwrap_or_else(|_| "[serialize_error]".to_owned());
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact,
                                &self.blob_store,
                                &call_trace_id,
                                "arguments",
                                &mut truncated,
                                &mut offloaded,
                            )
                            .await,
                        )
                    }
                    None => None,
                }
            } else {
                None
            };
            let result_str = if capture_result {
                match response.result.as_ref() {
                    Some(v) => {
                        let raw = serde_json::to_string(v)
                            .unwrap_or_else(|_| "[serialize_error]".to_owned());
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact,
                                &self.blob_store,
                                &call_trace_id,
                                "result",
                                &mut truncated,
                                &mut offloaded,
                            )
                            .await,
                        )
                    }
                    None => None,
                }
            } else {
                None
            };
            let status = if arg_str.is_none() && result_str.is_none() {
                "disabled"
            } else if offloaded {
                // Same dominant-status rule as the AI gateway: a single
                // emit carrying one offloaded field reports "offloaded"
                // even if another field was small enough to truncate.
                "offloaded"
            } else if truncated {
                "truncated"
            } else {
                "captured"
            };
            (arg_str, result_str, Some(status.to_owned()))
        };

        let mut entry =
            actor
                .audit("tools.call")
                .trace_id(call_trace_id)
                .detail(serde_json::json!({
                    "server_id": server_id.to_string(),
                    "server_name": server_name,
                    "tool_name": tool_name,
                    "arguments": logged_arguments,
                    "duration_ms": started.elapsed().as_millis() as i64,
                    "status": status,
                    "error_message": error_message,
                }));
        if let Some(a) = arg_str {
            entry = entry.request_body(a);
        }
        if let Some(r) = result_str {
            entry = entry.response_body(r);
        }
        if let Some(s) = capture_status {
            entry = entry.body_capture_status(s);
        }
        self.audit.log(entry);

        response
    }
}
