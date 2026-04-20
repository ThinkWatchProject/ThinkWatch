use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use think_watch_common::limits::{
    self, RateLimitRule, RateLimitSubject, RateMetric, Surface, SurfaceConstraints, sliding,
};

use crate::access_control::is_tool_allowed;
use crate::cache::McpResponseCache;
use crate::circuit_breaker::McpCircuitBreakers;
use crate::pool::ConnectionPool;
use crate::registry::Registry;
use crate::session::SessionManager;

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
    /// Redis-backed response cache for MCP tool calls.  Only used for
    /// servers that don't forward per-user identity headers.
    pub cache: McpResponseCache,
    pub db: PgPool,
    pub redis: fred::clients::Client,
    pub dynamic_config: std::sync::Arc<think_watch_common::dynamic_config::DynamicConfig>,
    /// Audit sink — populated by the server when it constructs the
    /// proxy. One `mcp_logs` row per tools/call completion, tagged
    /// with trace_id so the /api/admin/trace view can correlate it
    /// with the AI-gateway row that triggered the call.
    pub audit: think_watch_common::audit::AuditLogger,
}

impl McpProxy {
    pub fn new(
        registry: Registry,
        pool: ConnectionPool,
        sessions: SessionManager,
        db: PgPool,
        redis: fred::clients::Client,
        dynamic_config: std::sync::Arc<think_watch_common::dynamic_config::DynamicConfig>,
        audit: think_watch_common::audit::AuditLogger,
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
            "tools/list" => self.handle_tools_list(ctx.allowed_mcp_tools, request).await,
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
        allowed_mcp_tools: Option<&[String]>,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let all_tools = self.registry.get_all_tools(None).await;

        let tools: Vec<serde_json::Value> = all_tools
            .into_iter()
            .filter(|(name, _)| is_tool_allowed(allowed_mcp_tools, name))
            .map(|(namespaced_name, info)| {
                serde_json::json!({
                    "name": namespaced_name,
                    "description": info.description.unwrap_or_default(),
                    "inputSchema": info.input_schema.unwrap_or(serde_json::json!({"type": "object"})),
                })
            })
            .collect();

        ok_response(request.id, serde_json::json!({ "tools": tools }))
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

        // When the server forwards caller identity ({{user_id}} etc.),
        // scope cache entries per-user so results are never leaked across
        // users.  Shared servers get a user-agnostic cache lane.
        let cache_user_id = if server.forwards_user_identity {
            Some(&user_id)
        } else {
            None
        };

        if effective_cache_ttl > 0 {
            if let Some(cached) = self
                .cache
                .get(&server.id, cache_user_id, &upstream_request)
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

        let response = match self
            .pool
            .send_request(
                &conn,
                &upstream_request,
                Some(&caller),
                upstream_sid.as_deref(),
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

                // JSON-RPC error responses still count as failures so the
                // breaker reflects upstream tool errors, not just transport
                // errors. We treat any `error` field as a failure.
                //
                // The `record_cb_with_kind` call inside the breaker fires
                // the global OPEN_LISTENER installed by the server, which
                // emits `provider.circuit_open` audit events uniformly
                // for AI and MCP backends — no per-call emission here.
                if resp.error.is_some() {
                    self.circuit_breakers.record_failure(&server_name).await;
                } else {
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
                    cache_user_id,
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
        let mut entry = think_watch_common::audit::AuditEntry::mcp("tools.call")
            .trace_id(call_trace_id)
            .detail(serde_json::json!({
                "server_id": server_id.to_string(),
                "server_name": server_name,
                "tool_name": tool_name,
                "duration_ms": started.elapsed().as_millis() as i64,
                "status": status,
                "error_message": error_message,
            }));
        entry = entry.user_id(user_id);
        self.audit.log(entry);

        response
    }
}
