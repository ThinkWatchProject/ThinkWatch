use sqlx::PgPool;
use uuid::Uuid;

use think_watch_common::lifecycle::stages::run_post_invoke;
use think_watch_common::lifecycle::state::{Authorized, CapturedView, Invocation, Invoked};
use think_watch_common::limits::SurfaceConstraints;

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

mod audit;
mod jsonrpc;
mod streaming;

use jsonrpc::ok_response;
pub use jsonrpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    METHOD_NOT_FOUND, NEEDS_USER_CREDENTIALS, err_response,
};
use streaming::build_replay_payload;
pub(crate) use streaming::pick_response_envelope;
pub use streaming::{HandleOutcome, StreamingPayload};

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
    /// Whether the downstream client signalled (via `Accept:
    /// text/event-stream`) that it can consume an SSE response. When
    /// `true` AND the upstream uses SSE, the proxy forwards chunks
    /// AS THEY ARRIVE instead of buffering — the client sees
    /// `notifications/progress` events the moment the upstream emits
    /// them. When `false` the existing buffered path runs (single
    /// JSON-RPC response). Methods other than `tools/call` always
    /// run buffered; the transport layer wraps their reply as a
    /// single SSE event if the client asked for SSE.
    pub wants_streaming: bool,
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
    /// Hot-swappable at-rest PII redactor for the audit body capture
    /// pipeline. Same handle the gateway audit path consumes — both
    /// surfaces load from `security.pii_redactor_patterns` so a rule
    /// edit in the admin UI applies to MCP and gateway simultaneously.
    pub blob_redactor: std::sync::Arc<arc_swap::ArcSwap<think_watch_common::pii::BlobRedactor>>,
}

impl McpProxy {
    #[allow(clippy::too_many_arguments)]
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
        blob_redactor: std::sync::Arc<arc_swap::ArcSwap<think_watch_common::pii::BlobRedactor>>,
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
            blob_redactor,
        }
    }

    /// Main entry point: dispatch a single JSON-RPC request from a client.
    /// `user_roles` is required because the access controller is now
    /// default-deny — without role information non-admin users would be
    /// rejected even when an explicit per-tool policy permits them.
    ///
    /// `tools/call` is the ONLY method that may upgrade to a streaming
    /// outcome (when both the client signalled `wants_streaming` AND
    /// the upstream replied with `text/event-stream`). Every other
    /// method stays buffered — the transport wraps as a single SSE
    /// event when the client wants SSE.
    pub async fn handle_request(
        &self,
        ctx: &RequestContext<'_>,
        request: JsonRpcRequest,
    ) -> HandleOutcome {
        match request.method.as_str() {
            "initialize" => HandleOutcome::Buffered(self.handle_initialize(request).await),
            "tools/list" => HandleOutcome::Buffered(self.handle_tools_list(ctx, request).await),
            "tools/call" => self.handle_tools_call(ctx, request).await,
            _ => HandleOutcome::Buffered(err_response(
                request.id,
                METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
            )),
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
                // Tools/list isn't a streaming surface — discard the
                // optional audit body unconditionally.
                Ok((r, _, _)) => r,
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
    ) -> HandleOutcome {
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
                return HandleOutcome::Buffered(err_response(
                    request.id,
                    INVALID_PARAMS,
                    "Missing params for tools/call",
                ));
            }
        };

        let namespaced_name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return HandleOutcome::Buffered(err_response(
                    request.id,
                    INVALID_PARAMS,
                    "Missing or invalid 'name' in params",
                ));
            }
        };

        // Resolve server + original tool name from the namespaced identifier.
        let (server, original_tool_name) =
            match self.registry.find_server_for_tool(namespaced_name).await {
                Some(pair) => pair,
                None => {
                    return HandleOutcome::Buffered(err_response(
                        request.id,
                        INVALID_PARAMS,
                        format!("Unknown tool: {namespaced_name}"),
                    ));
                }
            };

        // Rate-limit pre-flight — runs the lifecycle's shared
        // `check_limits` stage. The stage emits its own audit row
        // on short-circuit and the response shape comes from
        // `McpSurface::rate_limited_response` so the deny payload
        // stays identical to the pre-migration version. We bind
        // the `request.id` onto the response after the fact
        // because the stage doesn't know the wire-level id.
        let rules = crate::lifecycle::rate_limit_rules(surface_constraints, user_id);
        let fail_closed = self.dynamic_config.rate_limit_fail_closed().await;
        let raw = think_watch_common::lifecycle::state::Raw::<crate::lifecycle::McpSurface>::new(
            crate::lifecycle::McpIdentity {
                user_id,
                user_email: user_email.to_owned(),
                ip_address: ctx.ip_address.map(|s| s.to_owned()),
                surface_constraints: surface_constraints.clone(),
                allowed_mcp_tools: allowed_mcp_tools.map(<[String]>::to_vec),
            },
            trace_id.to_owned(),
            ctx.ip_address.map(|s| s.to_owned()),
        );
        let limits_checked = match think_watch_common::lifecycle::stages::check_limits::<
            crate::lifecycle::McpSurface,
        >(raw, &rules, &self.redis, fail_closed, &self.audit)
        .await
        {
            Ok(s) => s,
            Err(mut resp) => {
                // Bind the inbound JSON-RPC `id` so the client
                // can correlate the deny response to its request.
                resp.id = request.id;
                return HandleOutcome::Buffered(resp);
            }
        };

        // Access control via the shared `check_access` stage. The
        // candidate is the namespaced tool name we extracted at the
        // top of the handler; the surface impl
        // (`McpSurface::is_access_allowed`) calls `is_tool_allowed`
        // with the identity's `allowed_mcp_tools` patterns.
        let authorized = match think_watch_common::lifecycle::stages::check_access::<
            crate::lifecycle::McpSurface,
        >(limits_checked, namespaced_name, &self.audit)
        .await
        {
            Ok(s) => s,
            Err(mut resp) => {
                resp.id = request.id;
                return HandleOutcome::Buffered(resp);
            }
        };

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

        // Resolve the effective cache TTL (per-server override
        // wins; 0 = explicitly disabled) and the cache scope
        // (Global vs PerCaller with optional account label —
        // PerCaller without an override collapses to per-user, so
        // switching defaults grants at most TTL seconds of stale
        // cache).
        let effective_cache_ttl = server
            .cache_ttl_secs
            .unwrap_or(self.dynamic_config.mcp_cache_ttl_secs().await);
        let cache_scope = match server.cache_scope {
            ServerCacheScope::Global => None,
            ServerCacheScope::PerCaller => Some(CallerScope {
                user_id: &user_id,
                account_label: account_label_for_server(ctx.mcp_account_overrides, server.id),
            }),
        };

        // Cache lookup + circuit-breaker gate via the MCP-specific
        // lifecycle stages. Each short-circuits with the
        // appropriate response shape (cached body on hit;
        // INTERNAL_ERROR with the unavailable message on breaker
        // open) AND emits a `tools.call.cache_hit` /
        // `tools.call.breaker_open` audit row so the deny shows up
        // on the trace UI — the pre-migration inline code was
        // silent on both.
        let authorized = match crate::lifecycle::stages::check_cache(
            authorized,
            &self.cache,
            server.id,
            cache_scope,
            effective_cache_ttl,
            &upstream_request,
            &self.audit,
        )
        .await
        {
            Ok(s) => s,
            Err(mut cached) => {
                cached.id = request.id;
                return HandleOutcome::Buffered(cached);
            }
        };
        let authorized = match crate::lifecycle::stages::check_breaker(
            authorized,
            &self.circuit_breakers,
            &server.name,
            &self.audit,
        )
        .await
        {
            Ok(s) => s,
            Err(mut resp) => {
                resp.id = request.id;
                return HandleOutcome::Buffered(resp);
            }
        };

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
                return HandleOutcome::Buffered(JsonRpcResponse {
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
                });
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
                return HandleOutcome::Buffered(JsonRpcResponse {
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
                });
            }
            Err(ResolverError::RefreshFailed {
                kind: RefreshFailureKind::Transient,
                message,
                ..
            }) => {
                return HandleOutcome::Buffered(err_response(
                    request.id.clone(),
                    INTERNAL_ERROR,
                    format!(
                        "Upstream OAuth provider for MCP server '{server_name}' is \
                         temporarily unavailable ({message}). Retry in a few seconds."
                    ),
                ));
            }
            Err(e) => {
                tracing::error!(
                    server_id = %server_id,
                    error = %e,
                    "credential resolver failed"
                );
                return HandleOutcome::Buffered(err_response(
                    request.id.clone(),
                    INTERNAL_ERROR,
                    format!("Credential resolver failed: {e}"),
                ));
            }
        };
        let auth_ref = auth_header.as_ref().map(AuthInjection::as_pair);

        // The post-invoke pipeline owns breaker / cache / audit work
        // for both transports below — build the deps once, hand them
        // to `run_post_invoke` per branch.
        let logged_arguments = params.get("arguments").cloned();
        let mut deps = crate::lifecycle::McpPostInvokeDeps {
            proxy: self.clone(),
            server_id,
            server_name: server_name.clone(),
            tool_name: tool_name.clone(),
            upstream_request: upstream_request.clone(),
            logged_arguments,
            cache_scope_kind: server.cache_scope,
            cache_account_label: account_label_for_server(ctx.mcp_account_overrides, server.id)
                .map(|s| s.to_owned()),
            effective_cache_ttl,
            // Filled in for the buffered branch when `send_request`
            // captured an upstream SSE timeline; stays None for the
            // streaming branch (timeline lives in CapturedView).
            stream_audit_body: None,
            original_request_id: request.id.clone(),
        };

        // Destructure the Authorized state. Each `Invocation`
        // constructor below moves these carry-over fields into the
        // new `Invoked` / `PumpContext`. The branches are mutually
        // exclusive — Rust's move checker accepts the repeated
        // identifiers in different match / if arms because exactly
        // one path runs per request.
        let Authorized {
            identity: auth_identity,
            trace_id: auth_trace_id,
            started_at: auth_started_at,
            client_ip: auth_client_ip,
            limit_check: auth_limit_check,
            access_candidate: auth_access_candidate,
        } = authorized;

        // Compose the upstream invocation. Two transports
        // (`send_request_streaming` for real SSE pass-through,
        // `send_request` for buffered JSON-RPC) converge on
        // [`Invocation`] — Buffered when no chunks flow downstream,
        // Streaming when the upstream upgraded to text/event-stream.
        // Breaker accounting for transport errors lives in the
        // `record_outcome` hook: an `err_response(INTERNAL_ERROR)`
        // is classified as a server-side failure by
        // `record_breaker_for_response`, so the explicit
        // `record_failure` calls the pre-migration code had on each
        // error branch are absorbed into the single hook call site.
        let invocation: Invocation<crate::lifecycle::McpSurface> = if ctx.wants_streaming {
            match self
                .pool
                .send_request_streaming(
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
                    let is_sse = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_lowercase().contains("text/event-stream"))
                        .unwrap_or(false);
                    // Persist the upstream session BEFORE returning so
                    // a follow-up call from the same client can reuse
                    // it even while the current stream is still in
                    // flight.
                    if let Some(sid) = new_upstream_sid {
                        self.sessions
                            .set_upstream_session(client_session_id, server_id, sid)
                            .await;
                    }
                    if is_sse {
                        self.build_mcp_pump(
                            resp,
                            crate::proxy::streaming::PumpContext {
                                identity: auth_identity,
                                trace_id: auth_trace_id,
                                started_at: auth_started_at,
                                client_ip: auth_client_ip,
                                limit_check: auth_limit_check,
                                access_candidate: auth_access_candidate,
                            },
                        )
                    } else {
                        // Upstream chose application/json — buffer it.
                        let response = match resp.json::<JsonRpcResponse>().await {
                            Ok(r) => r,
                            Err(e) => err_response(
                                request.id.clone(),
                                INTERNAL_ERROR,
                                format!("Upstream server error: parse failed: {e}"),
                            ),
                        };
                        Invocation::Buffered(Invoked {
                            identity: auth_identity,
                            trace_id: auth_trace_id,
                            started_at: auth_started_at,
                            client_ip: auth_client_ip,
                            limit_check: auth_limit_check,
                            access_candidate: auth_access_candidate,
                            view: CapturedView::Buffered(response),
                        })
                    }
                }
                Err(e) => {
                    tracing::error!(
                        server_id = %server_id,
                        error = %e,
                        "streaming upstream tools/call failed"
                    );
                    let response = err_response(
                        request.id.clone(),
                        INTERNAL_ERROR,
                        format!("Upstream server error: {e}"),
                    );
                    Invocation::Buffered(Invoked {
                        identity: auth_identity,
                        trace_id: auth_trace_id,
                        started_at: auth_started_at,
                        client_ip: auth_client_ip,
                        limit_check: auth_limit_check,
                        access_candidate: auth_access_candidate,
                        view: CapturedView::Buffered(response),
                    })
                }
            }
        } else {
            match self
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
                Ok((resp, new_upstream_sid, stream_body)) => {
                    if let Some(sid) = new_upstream_sid {
                        self.sessions
                            .set_upstream_session(client_session_id, server_id, sid)
                            .await;
                    }
                    // The pool captured an upstream SSE timeline when
                    // present — thread it into the audit row via deps
                    // so the buffered emit_audit still shows the full
                    // timeline.
                    deps.stream_audit_body = stream_body;
                    Invocation::Buffered(Invoked {
                        identity: auth_identity,
                        trace_id: auth_trace_id,
                        started_at: auth_started_at,
                        client_ip: auth_client_ip,
                        limit_check: auth_limit_check,
                        access_candidate: auth_access_candidate,
                        view: CapturedView::Buffered(resp),
                    })
                }
                Err(e) => {
                    tracing::error!(
                        server_id = %server_id,
                        error = %e,
                        "upstream tools/call failed"
                    );
                    let response = err_response(
                        request.id.clone(),
                        INTERNAL_ERROR,
                        format!("Upstream server error: {e}"),
                    );
                    Invocation::Buffered(Invoked {
                        identity: auth_identity,
                        trace_id: auth_trace_id,
                        started_at: auth_started_at,
                        client_ip: auth_client_ip,
                        limit_check: auth_limit_check,
                        access_candidate: auth_access_candidate,
                        view: CapturedView::Buffered(response),
                    })
                }
            }
        };

        // Dispatch into the post-invoke pipeline. Buffered runs
        // synchronously; streaming spawns a detached task so chunks
        // flow downstream while breaker / cache / audit work runs as
        // the tail resolves.
        match invocation {
            Invocation::Buffered(invoked) => {
                let emitted = run_post_invoke::<crate::lifecycle::McpSurface>(invoked, &deps).await;
                let response = emitted
                    .response
                    .expect("Invocation::Buffered always yields Emitted.response");
                if ctx.wants_streaming {
                    // Client signalled SSE but the upstream replied
                    // buffered. Wrap the response (and any captured
                    // upstream timeline) as a sequence of SSE events.
                    HandleOutcome::Streaming(build_replay_payload(
                        response,
                        deps.stream_audit_body.as_deref(),
                    ))
                } else {
                    HandleOutcome::Buffered(response)
                }
            }
            Invocation::Streaming { response, tail } => {
                tokio::spawn(async move {
                    let invoked = tail.await;
                    run_post_invoke::<crate::lifecycle::McpSurface>(invoked, &deps).await;
                });
                HandleOutcome::Streaming(response)
            }
        }
    }

    /// Update the per-server circuit breaker based on a completed
    /// JSON-RPC response.
    ///
    /// Counts as a failure only when the upstream returned a
    /// server-side error code:
    ///   - `-32603` (Internal error)
    ///   - `-32000` .. `-32099` (implementation-defined server errors)
    ///
    /// Everything else — success, parse error, invalid params,
    /// method not found, our own custom application codes — credits
    /// the breaker, since the upstream proved reachable AND the
    /// fault is caller-attributable, not provider-attributable.
    /// One open breaker per misbehaving client used to deny every
    /// other user on the same shared server for the full cooldown;
    /// this gate fixes that.
    pub(crate) async fn record_breaker_for_response(
        &self,
        server_name: &str,
        response: &JsonRpcResponse,
    ) {
        let is_server_failure = response
            .error
            .as_ref()
            .map(|err| {
                let c = err.code;
                c == INTERNAL_ERROR || (-32099..=-32000).contains(&c)
            })
            .unwrap_or(false);
        if is_server_failure {
            self.circuit_breakers.record_failure(server_name).await;
        } else {
            self.circuit_breakers.record_success(server_name).await;
        }
    }
}
