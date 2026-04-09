use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use think_watch_common::limits::{self, RateLimitSubject, RateMetric, sliding};

use crate::access_control::AccessController;
use crate::circuit_breaker::McpCircuitBreakers;
use crate::pool::ConnectionPool;
use crate::registry::Registry;

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
// McpProxy
// ---------------------------------------------------------------------------

/// The core MCP proxy.  Receives JSON-RPC requests from clients, aggregates
/// tool lists from all registered upstream servers, enforces access control,
/// and forwards tool calls to the correct upstream server.
///
/// Holds the same `db` / `redis` handles the AI gateway uses so the
/// shared `limits` engine can be queried per request without bouncing
/// out to a separate service. The previous in-process per-user rate
/// limiter (hardcoded 60 calls/min) has been replaced — quotas now
/// live in `rate_limit_rules` and are configurable per user / per
/// MCP server.
#[derive(Clone)]
pub struct McpProxy {
    pub registry: Registry,
    pub access_controller: AccessController,
    pub pool: ConnectionPool,
    pub circuit_breakers: McpCircuitBreakers,
    pub db: PgPool,
    pub redis: fred::clients::Client,
}

impl McpProxy {
    pub fn new(
        registry: Registry,
        access_controller: AccessController,
        pool: ConnectionPool,
        db: PgPool,
        redis: fred::clients::Client,
    ) -> Self {
        Self {
            registry,
            access_controller,
            pool,
            circuit_breakers: McpCircuitBreakers::new(),
            db,
            redis,
        }
    }

    /// Main entry point: dispatch a single JSON-RPC request from a client.
    /// `user_roles` is required because the access controller is now
    /// default-deny — without role information non-admin users would be
    /// rejected even when an explicit per-tool policy permits them.
    pub async fn handle_request(
        &self,
        user_id: Uuid,
        user_roles: &[String],
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => self.handle_initialize(request).await,
            "tools/list" => self.handle_tools_list(user_id, user_roles, request).await,
            "tools/call" => self.handle_tools_call(user_id, user_roles, request).await,
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
        user_id: Uuid,
        user_roles: &[String],
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let all_tools = self.registry.get_all_tools(None).await;

        let mut tools = Vec::new();
        for (namespaced_name, info) in all_tools {
            // Resolve the server so we can run an access check.
            if let Some((server, original_name)) =
                self.registry.find_server_for_tool(&namespaced_name).await
            {
                let allowed = self
                    .access_controller
                    .check_tool_access(user_id, server.id, &original_name, user_roles)
                    .await;
                if !allowed {
                    continue;
                }
            } else {
                // No server resolved → can't check policy → don't expose
                continue;
            }

            tools.push(serde_json::json!({
                "name": namespaced_name,
                "description": info.description.unwrap_or_default(),
                "inputSchema": info.input_schema.unwrap_or(serde_json::json!({"type": "object"})),
            }));
        }

        ok_response(request.id, serde_json::json!({ "tools": tools }))
    }

    // -----------------------------------------------------------------------
    // tools/call
    // -----------------------------------------------------------------------

    async fn handle_tools_call(
        &self,
        user_id: Uuid,
        user_roles: &[String],
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
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

        // Rate limit pre-flight against the new generic engine.
        // Subjects: (user, mcp_server). API keys aren't a thing on
        // the MCP gateway today — auth is JWT-only — so the api_key
        // subject is skipped. Only `requests`-metric rules are
        // checked here; the MCP path doesn't have a tokens metric.
        let subjects: Vec<(RateLimitSubject, Uuid)> = vec![
            (RateLimitSubject::User, user_id),
            (RateLimitSubject::McpServer, server.id),
        ];
        let rules = limits::list_enabled_rules_for_subjects(&self.db, &subjects)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("MCP rate-limit DB load failed: {e}; allowing call");
                Vec::new()
            });
        let resolved = sliding::resolve_rules(&rules, RateMetric::Requests);
        if !resolved.is_empty() {
            let outcome = sliding::check_and_record(&self.redis, &resolved, 1)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("MCP rate-limit redis error: {e}; allowing call");
                    sliding::CheckOutcome {
                        allowed: true,
                        exceeded_index: -1,
                        currents: Vec::new(),
                    }
                });
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

        // Access control check (default-deny: requires explicit policy
        // for non-admin users).
        let allowed = self
            .access_controller
            .check_tool_access(user_id, server.id, &original_tool_name, user_roles)
            .await;
        if !allowed {
            return err_response(request.id, INVALID_REQUEST, "Access denied for this tool");
        }

        // Build the upstream request with the original (un-namespaced) tool
        // name.
        let mut upstream_params = params.clone();
        if let Some(obj) = upstream_params.as_object_mut() {
            obj.insert(
                "name".to_owned(),
                serde_json::Value::String(original_tool_name),
            );
        }

        let upstream_request = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: request.id.clone(),
            method: "tools/call".to_owned(),
            params: Some(upstream_params),
        };

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
        match self.pool.send_request(&conn, &upstream_request).await {
            Ok(resp) => {
                // JSON-RPC error responses still count as failures so the
                // breaker reflects upstream tool errors, not just transport
                // errors. We treat any `error` field as a failure.
                if resp.error.is_some() {
                    self.circuit_breakers.record_failure(&server.name).await;
                } else {
                    self.circuit_breakers.record_success(&server.name).await;
                }
                resp
            }
            Err(e) => {
                self.circuit_breakers.record_failure(&server.name).await;
                tracing::error!(
                    server_id = %server.id,
                    error = %e,
                    "upstream tools/call failed"
                );
                err_response(
                    request.id,
                    INTERNAL_ERROR,
                    format!("Upstream server error: {e}"),
                )
            }
        }
    }
}
