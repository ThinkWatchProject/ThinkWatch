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

/// Body-capture truncation + offload + at-rest PII redaction for
/// `mcp_logs.tool_arguments` / `tool_result`. Mirrors the gateway-
/// side `process_body` semantics so both audit pipelines share the
/// same char-boundary-safe truncation + offload contract; redaction
/// flows through the shared `common::pii::BlobRedactor` so an
/// operator rule edit applies to both surfaces atomically.
#[allow(clippy::too_many_arguments)]
async fn apply_mcp_body_capture(
    mut s: String,
    max_bytes: usize,
    redact_pii: bool,
    blob_redactor: &think_watch_common::pii::BlobRedactor,
    blob_store: &std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    trace_id: &str,
    field: &'static str,
    truncated_flag: &mut bool,
    offloaded_flag: &mut bool,
) -> String {
    // Redact BEFORE the size check so the cap measures the version
    // that will actually land in audit storage — operators expect
    // `audit.body_max_bytes` to bound what's WRITTEN, not what was
    // sent.
    if redact_pii && !blob_redactor.is_empty() {
        s = blob_redactor.redact_blob(&s);
    }
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
    // Same operator-facing truncation signal as the gateway side.
    // Pair with `audit_body_offload_failed_total` in dashboards to
    // distinguish "S3 down" from "no S3 configured".
    metrics::counter!(
        "audit_body_truncated_total",
        "field" => field.to_string(),
    )
    .increment(1);
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

/// Find the byte index in `s` immediately AFTER a complete SSE
/// event terminator (`\n\n` or `\r\n\r\n`). Returns `None` when no
/// terminator has arrived yet so the caller knows to keep buffering.
fn find_sse_event_terminator(s: &str) -> Option<usize> {
    let lf = s.find("\n\n").map(|i| i + 2);
    let crlf = s.find("\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Concatenate the `data:` payload(s) of one SSE event block. Per the
/// SSE spec, multiple `data:` lines in one event join with `\n`; non-
/// `data:` lines (`event:`, `id:`, `retry:`, comments) are ignored.
/// Returns `None` when the event carried no data payload — caller
/// skips it rather than yielding an empty downstream event.
fn extract_sse_data_payload(event_block: &str) -> Option<String> {
    let mut out = String::new();
    let mut had_data = false;
    for line in event_block.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            if had_data {
                out.push('\n');
            }
            out.push_str(payload.trim_start());
            had_data = true;
        }
    }
    had_data.then_some(out)
}

/// Pick the JSON-RPC response envelope from a sequence of upstream
/// events, mirroring `pool::parse_sse_json_rpc`'s id-matching rules so
/// the streaming and buffered paths agree on what counts as "the
/// response" (vs `notifications/progress` events that the upstream
/// emitted during tool execution).
fn pick_response_envelope(
    events: &[serde_json::Value],
    request_id: Option<&serde_json::Value>,
) -> Option<JsonRpcResponse> {
    let pick = |with_id: bool| -> Option<serde_json::Value> {
        events
            .iter()
            .rev()
            .find(|e| {
                let has_result_or_error = e.get("result").is_some() || e.get("error").is_some();
                if !has_result_or_error {
                    return false;
                }
                if with_id {
                    request_id
                        .map(|rid| e.get("id").map(|id| id == rid).unwrap_or(false))
                        .unwrap_or(false)
                } else {
                    true
                }
            })
            .cloned()
    };
    let matched = pick(true).or_else(|| pick(false))?;
    serde_json::from_value(matched).ok()
}

/// How an upstream stream terminated. Drives audit-time classification
/// and circuit breaker accounting in the detached on-done task.
enum StreamOutcome {
    /// Stream drained to end-of-body without error.
    Natural,
    /// The underlying transport (bytes_stream) returned an error
    /// mid-flight, OR the upstream replied with a non-2xx before
    /// any chunks could be forwarded.
    UpstreamError { message: String },
    /// `done_tx` was dropped without sending — the producing future
    /// terminated before reaching its sentinel send, which happens
    /// when the downstream client disconnects and axum drops the
    /// SSE body. Treated as success for the breaker (no upstream
    /// fault) but as not-cacheable (we never saw the full response).
    ClientCancelled,
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

/// Outcome of `McpProxy::handle_request`. The transport layer
/// renders these differently:
///
/// * `Buffered` → either `Json(response)` (client wants JSON) or a
///   single SSE event wrapping `response` (client wants SSE);
/// * `Streaming` → an `axum::response::sse::Sse` body that pumps
///   upstream chunks downstream as they arrive, with audit emission
///   running in a detached `on_done` task.
pub enum HandleOutcome {
    Buffered(JsonRpcResponse),
    Streaming(StreamingPayload),
}

/// Wraps an SSE-shaped body the transport layer will hand to
/// `axum::response::sse::Sse::new`. `headers` carries the per-
/// request `Mcp-Session-Id` + `x-trace-id` that need to land on
/// the HTTP response.
pub struct StreamingPayload {
    pub body: std::pin::Pin<
        Box<
            dyn futures::stream::Stream<
                    Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                > + Send,
        >,
    >,
    pub new_session_id: Option<String>,
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
                            return HandleOutcome::Buffered(err_response(
                                request.id,
                                INVALID_REQUEST,
                                "Rate limited: rate_limiter_unavailable".to_string(),
                            ));
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
                return HandleOutcome::Buffered(err_response(
                    request.id,
                    INVALID_REQUEST,
                    format!("Rate limited: {label}"),
                ));
            }
        }

        // Access control: check tool against the user's allowed_mcp_tools patterns.
        if !is_tool_allowed(allowed_mcp_tools, namespaced_name) {
            return HandleOutcome::Buffered(err_response(
                request.id,
                INVALID_REQUEST,
                "Access denied for this tool",
            ));
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
                return HandleOutcome::Buffered(cached);
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
            return HandleOutcome::Buffered(err_response(
                request.id,
                INTERNAL_ERROR,
                format!(
                    "Upstream MCP server '{}' is temporarily unavailable",
                    server.name
                ),
            ));
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

        // Real chunk-by-chunk pass-through. Engages only when the
        // client signalled SSE capability AND the upstream returns
        // `text/event-stream`. Drives audit + cache + breaker from
        // a detached on-done task so each upstream chunk lands on
        // the client as it arrives, instead of after the upstream
        // completes. Application/json upstream falls through to the
        // synchronous tail below (chunk parsing has nothing to do
        // since there are no chunk boundaries).
        let (response, stream_audit_body) = if ctx.wants_streaming {
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
                    if is_sse {
                        // Persist the upstream session BEFORE returning
                        // the streaming body so a follow-up call from
                        // the same client can reuse it even while the
                        // current stream is still in flight.
                        if let Some(sid) = new_upstream_sid {
                            self.sessions
                                .set_upstream_session(client_session_id, server_id, sid)
                                .await;
                        }
                        let logged_arguments = params.get("arguments").cloned();
                        let cache_account_label =
                            account_label_for_server(ctx.mcp_account_overrides, server.id)
                                .map(|s| s.to_owned());
                        return HandleOutcome::Streaming(self.build_chunk_passthrough(
                            resp,
                            request.id.clone(),
                            user_id,
                            user_email.to_owned(),
                            ctx.ip_address.map(|s| s.to_owned()),
                            server_id,
                            server_name.clone(),
                            tool_name.clone(),
                            call_trace_id.clone(),
                            upstream_request.clone(),
                            logged_arguments,
                            started,
                            server.cache_scope,
                            cache_account_label,
                            effective_cache_ttl,
                        ));
                    }
                    // application/json upstream — buffer the body and
                    // fall through to the synchronous tail. Same logic
                    // the buffered-path Ok arm does, just inline here
                    // because we already consumed the request.
                    if let Some(sid) = new_upstream_sid {
                        self.sessions
                            .set_upstream_session(client_session_id, server_id, sid)
                            .await;
                    }
                    let parsed = resp.json::<JsonRpcResponse>().await;
                    let response = match parsed {
                        Ok(r) => r,
                        Err(e) => {
                            self.circuit_breakers.record_failure(&server_name).await;
                            err_response(
                                request.id.clone(),
                                INTERNAL_ERROR,
                                format!("Upstream server error: parse failed: {e}"),
                            )
                        }
                    };
                    self.record_breaker_for_response(&server_name, &response)
                        .await;
                    (response, None)
                }
                Err(e) => {
                    self.circuit_breakers.record_failure(&server_name).await;
                    tracing::error!(
                        server_id = %server_id,
                        error = %e,
                        "streaming upstream tools/call failed"
                    );
                    (
                        err_response(
                            request.id.clone(),
                            INTERNAL_ERROR,
                            format!("Upstream server error: {e}"),
                        ),
                        None,
                    )
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
                    // Persist any upstream session ID the server returned so
                    // subsequent calls from this user reuse the same session.
                    if let Some(sid) = new_upstream_sid {
                        self.sessions
                            .set_upstream_session(client_session_id, server_id, sid)
                            .await;
                    }

                    self.record_breaker_for_response(&server_name, &resp).await;
                    (resp, stream_body)
                }
                Err(e) => {
                    self.circuit_breakers.record_failure(&server_name).await;
                    tracing::error!(
                        server_id = %server_id,
                        error = %e,
                        "upstream tools/call failed"
                    );
                    (
                        err_response(
                            request.id.clone(),
                            INTERNAL_ERROR,
                            format!("Upstream server error: {e}"),
                        ),
                        // Transport error path — no stream events were
                        // successfully received, so no audit body to embed.
                        None,
                    )
                }
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

        // Capture the call arguments alongside the tool name so the
        // trace endpoint can show what was actually invoked.
        let logged_arguments = params.get("arguments").cloned();
        self.emit_tools_call_audit(
            user_id,
            user_email,
            ctx.ip_address,
            server_id,
            &server_name,
            &tool_name,
            &call_trace_id,
            logged_arguments.as_ref(),
            started,
            &response,
            stream_audit_body.as_deref(),
        )
        .await;

        if ctx.wants_streaming {
            // Client signalled SSE capability via `Accept: text/event-stream`.
            // The audit pipeline ran in full above (single, deterministic
            // pass) — what's left is shape conversion: turn the response
            // into a sequence of SSE events the client expects.
            //
            // When the upstream itself used text/event-stream we have
            // `stream_audit_body` — a JSON array of every event envelope
            // (progress notifications + final response) the upstream
            // emitted. Replay each one as a discrete SSE event so the
            // client sees the full timeline instead of a single buffered
            // response. Note: this is NOT real-time pass-through (we
            // already buffered the entire upstream before audit), it's
            // the lossless replay of what we received. Real-time
            // chunk-by-chunk forwarding is a follow-up that needs to
            // restructure the audit emit timing.
            //
            // When the upstream replied with plain application/json we
            // get one synthesized event carrying the response envelope.
            HandleOutcome::Streaming(build_replay_payload(response, stream_audit_body.as_deref()))
        } else {
            HandleOutcome::Buffered(response)
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
    async fn record_breaker_for_response(&self, server_name: &str, response: &JsonRpcResponse) {
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

    /// Emit the `mcp_logs` audit row for a completed tools/call.
    ///
    /// Centralised so the buffered and (future) chunk-by-chunk
    /// streaming paths share a single, deterministic audit pipeline —
    /// the streaming path calls this once from its detached on_done
    /// task after the upstream stream completes (or the client
    /// disconnects), guaranteeing the row is emitted exactly once
    /// regardless of how the call terminated.
    #[allow(clippy::too_many_arguments)]
    async fn emit_tools_call_audit(
        &self,
        user_id: Uuid,
        user_email: &str,
        ip_address: Option<&str>,
        server_id: Uuid,
        server_name: &str,
        tool_name: &str,
        call_trace_id: &str,
        logged_arguments: Option<&serde_json::Value>,
        started: std::time::Instant,
        response: &JsonRpcResponse,
        stream_audit_body: Option<&str>,
    ) {
        let (status, error_message) = if let Some(ref err) = response.error {
            ("error".to_string(), Some(err.message.clone()))
        } else {
            ("ok".to_string(), None)
        };
        use think_watch_common::audit::{AuditActor, BodyCaptureStatus, McpActor};
        let actor = McpActor {
            user_id,
            user_email,
            ip: ip_address,
        };

        // Body capture for audit. arguments + upstream result land in
        // dedicated `mcp_logs.tool_arguments` / `mcp_logs.tool_result`
        // columns (separate from the metadata-only `detail` JSON) so
        // auditors can query them without parsing JSON per row.
        let dc = &self.dynamic_config;
        let capture_args = dc.audit_capture_tool_arguments().await;
        let capture_result = dc.audit_capture_tool_results().await;
        let body_max = dc.audit_body_max_bytes().await as usize;
        let redact_pii = dc.audit_body_redact_pii().await;
        // Snapshot the hot-swappable redactor ONCE per request so the
        // arguments + result halves see the same pattern set even if
        // the operator hot-swaps mid-call.
        let blob_redactor_snapshot = self.blob_redactor.load_full();
        let (arg_str, arg_bytes, result_str, result_bytes, capture_status) = if !capture_args
            && !capture_result
        {
            (
                None,
                None,
                None,
                None,
                Some(BodyCaptureStatus::Disabled.as_str().to_owned()),
            )
        } else {
            let mut truncated = false;
            let mut offloaded = false;
            let mut arg_bytes: Option<u32> = None;
            let mut result_bytes: Option<u32> = None;
            let arg_str = if capture_args {
                match logged_arguments {
                    Some(v) => {
                        let raw = serde_json::to_string(v)
                            .unwrap_or_else(|_| "[serialize_error]".to_owned());
                        arg_bytes = Some(raw.len() as u32);
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact_pii,
                                &blob_redactor_snapshot,
                                &self.blob_store,
                                call_trace_id,
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
                // Prefer the FULL streaming-event sequence (when the
                // upstream used text/event-stream and emitted
                // progress notifications + a final response) over
                // just the final `result` field. Auditors replaying
                // a long-running tool execution need the whole
                // timeline, not just the punchline.
                let raw_opt: Option<String> = match (stream_audit_body, response.result.as_ref()) {
                    (Some(stream), _) => Some(stream.to_owned()),
                    (None, Some(v)) => Some(
                        serde_json::to_string(v).unwrap_or_else(|_| "[serialize_error]".to_owned()),
                    ),
                    (None, None) => None,
                };
                match raw_opt {
                    Some(raw) => {
                        result_bytes = Some(raw.len() as u32);
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact_pii,
                                &blob_redactor_snapshot,
                                &self.blob_store,
                                call_trace_id,
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
                BodyCaptureStatus::Disabled
            } else if offloaded {
                // Same dominant-status rule as the AI gateway: a
                // single emit carrying one offloaded field reports
                // `offloaded` even if another field was small enough
                // to truncate.
                BodyCaptureStatus::Offloaded
            } else if truncated {
                BodyCaptureStatus::Truncated
            } else {
                BodyCaptureStatus::Captured
            };
            (
                arg_str,
                arg_bytes,
                result_str,
                result_bytes,
                Some(status.as_str().to_owned()),
            )
        };

        let mut entry = actor
            .audit("tools.call")
            .trace_id(call_trace_id.to_owned())
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
        // Stamp ORIGINAL byte counts (pre-offload) so the audit row's
        // `arguments_bytes` / `result_bytes` columns reflect the
        // user's actual payload size. Without these, an offloaded
        // tool result would report ~80 bytes (the s3:// URL length)
        // and break "average tool result size" analytics.
        if let Some(b) = arg_bytes {
            entry = entry.request_body_bytes(b);
        }
        if let Some(b) = result_bytes {
            entry = entry.response_body_bytes(b);
        }
        if let Some(s) = capture_status {
            entry = entry.body_capture_status(s);
        }
        self.audit.log(entry);
    }

    /// Build a `StreamingPayload` that pumps upstream SSE chunks
    /// downstream AS THEY ARRIVE, while accumulating every event
    /// envelope into a shared buffer that a detached on-done task
    /// drains to run circuit breaker accounting, cache write, and
    /// audit emission exactly once when the stream terminates (or
    /// the client disconnects).
    ///
    /// The on-done task runs on graceful end-of-body (Natural), on
    /// a bytes_stream error (UpstreamError), or on client drop
    /// (ClientCancelled — `done_tx` is dropped when the producing
    /// future is). It is the single audit-emit site for this code
    /// path; the synchronous tail in `handle_tools_call` is skipped
    /// when we return here.
    #[allow(clippy::too_many_arguments)]
    fn build_chunk_passthrough(
        &self,
        upstream_resp: reqwest::Response,
        request_id: Option<serde_json::Value>,
        user_id: Uuid,
        user_email: String,
        ip_address: Option<String>,
        server_id: Uuid,
        server_name: String,
        tool_name: String,
        call_trace_id: String,
        upstream_request: JsonRpcRequest,
        logged_arguments: Option<serde_json::Value>,
        started: std::time::Instant,
        cache_scope_kind: ServerCacheScope,
        cache_account_label: Option<String>,
        effective_cache_ttl: u64,
    ) -> StreamingPayload {
        use futures::stream::StreamExt;
        use std::sync::{Arc, Mutex};

        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> =
            Arc::new(Mutex::new(Vec::with_capacity(8)));
        let events_for_done = events_buf.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();

        // The on-done task gets its own owned clone of every piece of
        // state it needs — McpProxy is `Clone` so the breaker / cache /
        // audit handles all come along for free.
        let proxy = self.clone();
        let server_name_done = server_name.clone();
        let request_id_done = request_id.clone();
        tokio::spawn(async move {
            let outcome = done_rx.await.unwrap_or(StreamOutcome::ClientCancelled);
            let events = events_for_done
                .lock()
                .ok()
                .map(|mut g| std::mem::take(&mut *g))
                .unwrap_or_default();
            let response = pick_response_envelope(&events, request_id_done.as_ref())
                .unwrap_or_else(|| {
                    let msg = match &outcome {
                        StreamOutcome::Natural => {
                            "Upstream stream ended without a response envelope".to_string()
                        }
                        StreamOutcome::UpstreamError { message } => {
                            format!("Upstream stream error: {message}")
                        }
                        StreamOutcome::ClientCancelled => {
                            "Client cancelled before upstream replied".to_string()
                        }
                    };
                    err_response(request_id_done.clone(), INTERNAL_ERROR, msg)
                });

            // Circuit breaker — transport error → failure; otherwise
            // follow the same server-side-vs-caller-side rule the
            // buffered path uses so the breaker doesn't open on a
            // single user's bad INVALID_PARAMS.
            match &outcome {
                StreamOutcome::UpstreamError { .. } => {
                    proxy
                        .circuit_breakers
                        .record_failure(&server_name_done)
                        .await;
                }
                StreamOutcome::Natural | StreamOutcome::ClientCancelled => {
                    proxy
                        .record_breaker_for_response(&server_name_done, &response)
                        .await;
                }
            }

            // Cache write only on a fully drained, successful stream.
            // Client cancellation means we may have a partial view of
            // the response so caching it would poison subsequent calls.
            if effective_cache_ttl > 0
                && response.error.is_none()
                && matches!(outcome, StreamOutcome::Natural)
            {
                let cache_scope = match cache_scope_kind {
                    ServerCacheScope::Global => None,
                    ServerCacheScope::PerCaller => Some(CallerScope {
                        user_id: &user_id,
                        account_label: cache_account_label.as_deref(),
                    }),
                };
                proxy
                    .cache
                    .set(
                        &server_id,
                        cache_scope,
                        &upstream_request,
                        &response,
                        effective_cache_ttl,
                    )
                    .await;
            }

            // Audit emit with the full upstream event timeline — same
            // shape the buffered path produces via parse_sse_json_rpc,
            // so trace replay UI gets identical data regardless of
            // which transport the call took.
            let stream_audit_body = serde_json::to_string(&events).ok();
            proxy
                .emit_tools_call_audit(
                    user_id,
                    &user_email,
                    ip_address.as_deref(),
                    server_id,
                    &server_name_done,
                    &tool_name,
                    &call_trace_id,
                    logged_arguments.as_ref(),
                    started,
                    &response,
                    stream_audit_body.as_deref(),
                )
                .await;
        });

        // The body itself: SSE chunk-by-chunk pass-through. Buffers
        // bytes only until the next `\n\n` boundary, then yields one
        // downstream event per upstream event. axum's `Sse` wrapper
        // re-frames each yielded `Event::default().data(payload)` as
        // `data: <payload>\n\n` on the wire.
        let bytes_source = upstream_resp
            .bytes_stream()
            .map(|r| r.map_err(|e| e.to_string()));
        let body = build_passthrough_body(bytes_source, events_buf, done_tx);

        StreamingPayload {
            body,
            // Proxy session-id is set by the transport layer from the
            // per-request session it owns; we don't override it here.
            new_session_id: None,
        }
    }
}

/// Pump bytes from `source` downstream as discrete SSE events,
/// accumulating each parsed envelope into `events_buf` for the
/// on-done audit pass. On natural end-of-stream sends `Natural` via
/// `done_tx`; on a source-error sends `UpstreamError`; if neither
/// path runs (e.g. the consumer drops the stream mid-flight) the
/// receiver sees `Err` and treats it as `ClientCancelled`.
///
/// Generic over the source so the production path (reqwest's
/// `bytes_stream` mapped to `String` errors) and tests (an
/// `iter`-backed Stream) share one implementation.
fn build_passthrough_body<S>(
    source: S,
    events_buf: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    done_tx: tokio::sync::oneshot::Sender<StreamOutcome>,
) -> std::pin::Pin<
    Box<
        dyn futures::stream::Stream<
                Item = Result<axum::response::sse::Event, std::convert::Infallible>,
            > + Send,
    >,
>
where
    S: futures::stream::Stream<Item = Result<bytes::Bytes, String>> + Send + 'static,
{
    use axum::response::sse::Event;
    use std::convert::Infallible;
    let body = async_stream::stream! {
        use futures::stream::StreamExt;
        let source = source;
        futures::pin_mut!(source);
        let mut text_buf = String::new();
        let mut done_tx = Some(done_tx);
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(bytes) => {
                    let s = String::from_utf8_lossy(&bytes);
                    text_buf.push_str(&s);
                    while let Some(end) = find_sse_event_terminator(&text_buf) {
                        let event_block: String = text_buf.drain(..end).collect();
                        if let Some(payload) = extract_sse_data_payload(&event_block)
                            && !payload.is_empty()
                        {
                            // Best-effort JSON parse so non-JSON
                            // `data:` payloads (rare) still land in
                            // the timeline verbatim.
                            let parsed = serde_json::from_str::<serde_json::Value>(&payload)
                                .unwrap_or_else(|_| {
                                    serde_json::Value::String(payload.clone())
                                });
                            if let Ok(mut g) = events_buf.lock() {
                                g.push(parsed);
                            }
                            yield Ok::<Event, Infallible>(Event::default().data(payload));
                        }
                    }
                }
                Err(message) => {
                    // Transport-level error mid-stream. Surface via
                    // the on-done task (no spec-defined error event
                    // shape to emit downstream).
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(StreamOutcome::UpstreamError { message });
                    }
                    break;
                }
            }
        }
        // Defensive flush: some upstreams omit the final `\n\n`.
        let trailing = text_buf.trim_end_matches(['\n', '\r']);
        if !trailing.is_empty()
            && let Some(payload) = extract_sse_data_payload(trailing)
            && !payload.is_empty()
        {
            let parsed = serde_json::from_str::<serde_json::Value>(&payload)
                .unwrap_or_else(|_| serde_json::Value::String(payload.clone()));
            if let Ok(mut g) = events_buf.lock() {
                g.push(parsed);
            }
            yield Ok::<Event, Infallible>(Event::default().data(payload));
        }
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(StreamOutcome::Natural);
        }
    };
    Box::pin(body)
}

/// Convert a (possibly already-streamed-by-upstream) JsonRpcResponse
/// into an SSE payload the transport layer can hand straight to
/// `axum::response::sse::Sse::new`.
///
/// `stream_audit_body` is the JSON array of upstream events the pool
/// captured in commit cb50ea3 — present when the upstream replied
/// with `text/event-stream`. We parse it back into discrete envelopes
/// and replay each as one SSE event. When absent, the upstream was
/// plain JSON and we emit one synthesized event with the response.
fn build_replay_payload(
    response: JsonRpcResponse,
    stream_audit_body: Option<&str>,
) -> StreamingPayload {
    use axum::response::sse::Event;
    use std::convert::Infallible;
    let mut events: Vec<String> = Vec::new();
    if let Some(audit_json) = stream_audit_body
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(audit_json)
    {
        for ev in arr {
            // Serialize each event back to a compact JSON line. The
            // SSE wire format wraps it in `data: ...\n\n` for us.
            if let Ok(s) = serde_json::to_string(&ev) {
                events.push(s);
            }
        }
    }
    // Fallback: no audit body OR parse failed — emit the final
    // response as a single event. Client still gets a spec-compliant
    // SSE shape with one envelope.
    if events.is_empty()
        && let Ok(s) = serde_json::to_string(&response)
    {
        events.push(s);
    }
    let stream = futures::stream::iter(
        events
            .into_iter()
            .map(|s| Ok::<_, Infallible>(Event::default().data(s))),
    );
    StreamingPayload {
        body: Box::pin(stream),
        new_session_id: None,
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use futures::StreamExt;

    fn make_response(id: u64, result: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(serde_json::json!(id)),
            result: Some(result),
            error: None,
        }
    }

    async fn drain(payload: StreamingPayload) -> Vec<String> {
        // SSE Event doesn't expose its data publicly. We rebuilt the
        // event from JSON strings on construction, so re-serializing
        // the stream via Debug isn't reliable. Instead the tests
        // assert COUNT of events and lean on the construction code
        // path being deterministic.
        let mut stream = payload.body;
        let mut count = Vec::new();
        while let Some(ev) = stream.next().await {
            // `Ok(Event)` — we yielded these — Display impl writes
            // the wire-format SSE payload (`data: ...\n\n`) so we can
            // sniff the body to confirm content survived round-trip.
            let serialized = format!("{:?}", ev.unwrap());
            count.push(serialized);
        }
        count
    }

    #[tokio::test]
    async fn plain_upstream_response_yields_single_event() {
        // No `stream_audit_body` → upstream replied with
        // application/json. We synthesize one event from the
        // response so the client still sees a spec-compliant SSE
        // shape.
        let resp = make_response(1, serde_json::json!({"content": "ok"}));
        let payload = build_replay_payload(resp, None);
        let events = drain(payload).await;
        assert_eq!(events.len(), 1, "single buffered response → one SSE event");
    }

    #[tokio::test]
    async fn streamed_upstream_replays_each_event() {
        // Three upstream events: two progress notifications + the
        // final response. All three should land downstream as
        // discrete SSE events.
        let audit_json = serde_json::to_string(&serde_json::json!([
            {"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":33}},
            {"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":66}},
            {"jsonrpc":"2.0","id":1,"result":{"content":"done"}}
        ]))
        .unwrap();
        let resp = make_response(1, serde_json::json!({"content": "done"}));
        let payload = build_replay_payload(resp, Some(&audit_json));
        let events = drain(payload).await;
        assert_eq!(
            events.len(),
            3,
            "every upstream SSE event should replay downstream"
        );
    }

    #[tokio::test]
    async fn malformed_audit_body_falls_back_to_response() {
        // If stream_audit_body is garbage (parse fails), we still
        // emit a single event with the response so the client gets
        // SOMETHING and not an empty SSE stream that hangs.
        let resp = make_response(1, serde_json::json!({"x": 1}));
        let payload = build_replay_payload(resp, Some("not json"));
        let events = drain(payload).await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn empty_audit_array_falls_back_to_response() {
        // Edge case: audit body parses but is `[]`. Still emit the
        // response — auditors expect at least the final envelope
        // to be visible to the client.
        let resp = make_response(1, serde_json::json!({"y": 2}));
        let payload = build_replay_payload(resp, Some("[]"));
        let events = drain(payload).await;
        assert_eq!(events.len(), 1);
    }
}

#[cfg(test)]
mod sse_parser_tests {
    use super::*;

    #[test]
    fn terminator_lf_lf() {
        assert_eq!(find_sse_event_terminator("data: x\n\n"), Some(9));
        assert_eq!(find_sse_event_terminator("data: x\n\nmore"), Some(9));
    }

    #[test]
    fn terminator_crlf_crlf() {
        assert_eq!(find_sse_event_terminator("data: x\r\n\r\n"), Some(11));
    }

    #[test]
    fn terminator_picks_first_boundary() {
        // `\n\n` appears earlier (at 6, end=8) than `\r\n\r\n` would.
        // The function returns the smaller index — whichever boundary
        // arrived first.
        let s = "a\nb\n\nc\r\n\r\n";
        assert_eq!(find_sse_event_terminator(s), Some(5));
    }

    #[test]
    fn terminator_none_when_incomplete() {
        assert_eq!(find_sse_event_terminator("data: still buffering"), None);
        // Single newline isn't a terminator — SSE requires the blank
        // line.
        assert_eq!(find_sse_event_terminator("data: x\n"), None);
    }

    #[test]
    fn extract_single_data_line() {
        assert_eq!(
            extract_sse_data_payload("data: hello"),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn extract_joins_multiple_data_lines() {
        // Per SSE spec: multi-line `data:` joins with `\n`.
        let block = "data: first\ndata: second";
        assert_eq!(
            extract_sse_data_payload(block),
            Some("first\nsecond".to_owned())
        );
    }

    #[test]
    fn extract_strips_optional_space_after_colon() {
        // `data: x` and `data:x` both yield `x`.
        assert_eq!(
            extract_sse_data_payload("data:no-space"),
            Some("no-space".to_owned())
        );
    }

    #[test]
    fn extract_ignores_other_sse_fields() {
        let block = "event: message\nid: 42\nretry: 1000\ndata: payload\n: comment";
        assert_eq!(extract_sse_data_payload(block), Some("payload".to_owned()));
    }

    #[test]
    fn extract_none_when_no_data_lines() {
        assert_eq!(extract_sse_data_payload("event: ping\nid: 1"), None);
    }

    #[test]
    fn pick_envelope_matches_request_id() {
        let req_id = serde_json::json!(7);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":50}}),
            serde_json::json!({"jsonrpc":"2.0","id":7,"result":{"content":"done"}}),
        ];
        let r = pick_response_envelope(&events, Some(&req_id)).expect("envelope");
        assert_eq!(r.id, Some(req_id));
        assert!(r.result.is_some());
    }

    #[test]
    fn pick_envelope_falls_back_to_last_response_shaped() {
        // Upstream replied with id=null on the final response (older
        // MCP impls do this). Match-by-id fails so we fall back to
        // the last envelope with result/error.
        let req_id = serde_json::json!(7);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":50}}),
            serde_json::json!({"jsonrpc":"2.0","id":null,"result":{"content":"done"}}),
        ];
        let r = pick_response_envelope(&events, Some(&req_id)).expect("fallback envelope");
        assert!(r.result.is_some());
    }

    #[test]
    fn pick_envelope_none_when_only_notifications() {
        let req_id = serde_json::json!(1);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":10}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":20}}),
        ];
        assert!(pick_response_envelope(&events, Some(&req_id)).is_none());
    }
}

#[cfg(test)]
mod passthrough_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream::{self, StreamExt};
    use std::sync::{Arc, Mutex};

    /// Build the body from an iterator of byte chunks and drive it
    /// to completion, returning the count of events that emerged
    /// downstream + the accumulated envelopes captured for audit +
    /// the resolved on-done outcome.
    async fn run_body(
        chunks: Vec<Result<Bytes, String>>,
    ) -> (usize, Vec<serde_json::Value>, Option<&'static str>) {
        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
        let source = stream::iter(chunks);
        let body = build_passthrough_body(source, events_buf.clone(), done_tx);
        let mut count = 0usize;
        let mut body = body;
        while let Some(_ev) = body.next().await {
            count += 1;
        }
        // Drain the oneshot — should always carry an outcome since the
        // body ran to completion (no early drop here).
        let outcome = done_rx.await.ok().map(|o| match o {
            StreamOutcome::Natural => "natural",
            StreamOutcome::UpstreamError { .. } => "upstream_error",
            StreamOutcome::ClientCancelled => "cancelled",
        });
        let captured = events_buf.lock().unwrap().clone();
        (count, captured, outcome)
    }

    #[tokio::test]
    async fn one_event_per_chunk() {
        // Each upstream chunk holds exactly one complete event. The
        // body should yield three downstream events and accumulate
        // three audit envelopes.
        let chunks = vec![
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":33}}\n\n",
            )),
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":66}}\n\n",
            )),
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":\"done\"}}\n\n",
            )),
        ];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 3, "three upstream events → three downstream events");
        assert_eq!(events.len(), 3);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn event_split_across_two_chunks() {
        // A single SSE event straddles the boundary between two byte
        // chunks. The buffer must hold the first chunk until the
        // terminator arrives, then yield once.
        let chunks = vec![
            Ok(Bytes::from("data: {\"jsonrpc\":\"2.0\",\"id\":")),
            Ok(Bytes::from("1,\"result\":{\"x\":1}}\n\n")),
        ];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1, "fragmented event must coalesce into one yield");
        assert_eq!(events.len(), 1);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        // Upstream coalesces two events into one byte chunk. The
        // inner while-loop should pump both out without waiting for
        // another chunk.
        let chunks = vec![Ok(Bytes::from("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"))];
        let (count, events, _outcome) = run_body(chunks).await;
        assert_eq!(count, 2, "back-to-back events in one chunk → two yields");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn trailing_event_without_final_terminator() {
        // Some upstreams end the body without a trailing `\n\n`. The
        // defensive flush at end-of-stream picks it up.
        let chunks = vec![Ok(Bytes::from(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":1}}",
        ))];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1, "trailing event flushed at EOF");
        assert_eq!(events.len(), 1);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn upstream_error_surfaces_via_oneshot() {
        // A mid-stream byte error breaks the loop and signals
        // UpstreamError to the on-done task. Earlier successful
        // chunks still land downstream.
        let chunks = vec![
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\n",
            )),
            Err("network reset".to_owned()),
        ];
        let (count, _events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1);
        assert_eq!(outcome, Some("upstream_error"));
    }

    #[tokio::test]
    async fn client_drop_yields_cancelled() {
        // Build a body that won't complete (infinite stream) and
        // drop it after one read. The oneshot sender is dropped
        // without sending → receiver sees Err → ClientCancelled.
        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
        // Stream that yields one event then would block forever in
        // production — for the test we just take(1) and drop.
        let source = stream::iter(vec![Ok::<Bytes, String>(Bytes::from(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\n",
        ))])
        .chain(stream::pending());
        let body = build_passthrough_body(source, events_buf.clone(), done_tx);
        let mut body = body;
        let _ = body.next().await;
        drop(body);
        // The sender lives inside the body's async_stream; dropping
        // the body drops the future, which drops the sender.
        let outcome = done_rx.await.ok();
        assert!(outcome.is_none(), "dropped sender → recv error");
        // The transport layer's wrapping spawn task interprets recv
        // error as ClientCancelled — verified separately in the
        // build_chunk_passthrough integration; here we just confirm
        // the signal.
    }

    #[tokio::test]
    async fn non_json_payload_recorded_verbatim() {
        // A non-JSON `data:` payload is rare but legal. It should
        // still land in the audit timeline as a string Value so the
        // trace isn't lossy, AND emerge downstream so the client
        // sees the same wire bytes the upstream emitted.
        let chunks = vec![Ok(Bytes::from("data: not-json-text\n\n"))];
        let (count, events, _) = run_body(chunks).await;
        assert_eq!(count, 1);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], serde_json::Value::String(_)));
    }
}
