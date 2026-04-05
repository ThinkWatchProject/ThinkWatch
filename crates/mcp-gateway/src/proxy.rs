use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::access_control::AccessController;
use crate::pool::ConnectionPool;
use crate::registry::Registry;

/// Per-user sliding window rate limiter for MCP tool calls.
#[derive(Clone)]
pub struct McpRateLimiter {
    /// user_id -> list of call timestamps (unix millis)
    windows: Arc<RwLock<HashMap<Uuid, Vec<i64>>>>,
    /// Maximum tool calls per user per minute
    max_calls_per_minute: u32,
}

impl McpRateLimiter {
    pub fn new(max_calls_per_minute: u32) -> Self {
        Self {
            windows: Arc::new(RwLock::new(HashMap::new())),
            max_calls_per_minute,
        }
    }

    /// Check if the user is within rate limits. Returns Ok if allowed.
    pub async fn check(&self, user_id: Uuid) -> Result<(), String> {
        let now = chrono::Utc::now().timestamp_millis();
        let window_start = now - 60_000;

        let mut windows = self.windows.write().await;
        let timestamps = windows.entry(user_id).or_default();

        // Remove expired entries
        timestamps.retain(|&ts| ts > window_start);

        if timestamps.len() >= self.max_calls_per_minute as usize {
            return Err(format!(
                "MCP tool call rate limit exceeded: {}/{} per minute",
                timestamps.len(),
                self.max_calls_per_minute
            ));
        }

        timestamps.push(now);
        Ok(())
    }

    /// Periodic cleanup of stale user entries.
    pub async fn cleanup(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        let window_start = now - 60_000;
        let mut windows = self.windows.write().await;
        windows.retain(|_, timestamps| {
            timestamps.retain(|&ts| ts > window_start);
            !timestamps.is_empty()
        });
    }
}

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
// McpProxy
// ---------------------------------------------------------------------------

/// The core MCP proxy.  Receives JSON-RPC requests from clients, aggregates
/// tool lists from all registered upstream servers, enforces access control,
/// and forwards tool calls to the correct upstream server.
#[derive(Clone)]
pub struct McpProxy {
    pub registry: Registry,
    pub access_controller: AccessController,
    pub pool: ConnectionPool,
    pub rate_limiter: McpRateLimiter,
}

impl McpProxy {
    pub fn new(
        registry: Registry,
        access_controller: AccessController,
        pool: ConnectionPool,
    ) -> Self {
        let rate_limiter = McpRateLimiter::new(60); // 60 tool calls per user per minute
        Self {
            registry,
            access_controller,
            pool,
            rate_limiter,
        }
    }

    /// Main entry point: dispatch a single JSON-RPC request from a client.
    pub async fn handle_request(&self, user_id: Uuid, request: JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => self.handle_initialize(request).await,
            "tools/list" => self.handle_tools_list(user_id, request).await,
            "tools/call" => self.handle_tools_call(user_id, request).await,
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
                "name": "AgentBastion MCP Gateway",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        ok_response(request.id, capabilities)
    }

    // -----------------------------------------------------------------------
    // tools/list
    // -----------------------------------------------------------------------

    async fn handle_tools_list(&self, user_id: Uuid, request: JsonRpcRequest) -> JsonRpcResponse {
        let all_tools = self.registry.get_all_tools(None).await;

        let mut tools = Vec::new();
        for (namespaced_name, info) in all_tools {
            // Resolve the server so we can run an access check.
            if let Some((server, original_name)) =
                self.registry.find_server_for_tool(&namespaced_name).await
            {
                let allowed = self
                    .access_controller
                    .check_tool_access_by_id(user_id, server.id, &original_name)
                    .await;
                if !allowed {
                    continue;
                }
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

    async fn handle_tools_call(&self, user_id: Uuid, request: JsonRpcRequest) -> JsonRpcResponse {
        // Per-user rate limiting for tool calls
        if let Err(msg) = self.rate_limiter.check(user_id).await {
            tracing::warn!(user_id = %user_id, "{msg}");
            return err_response(request.id, INVALID_REQUEST, msg);
        }

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

        // Access control check.
        let allowed = self
            .access_controller
            .check_tool_access_by_id(user_id, server.id, &original_tool_name)
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

        // Get (or create) a connection and forward the request.
        let conn = self.pool.get_or_create(&server).await;
        match self.pool.send_request(&conn, &upstream_request).await {
            Ok(resp) => resp,
            Err(e) => {
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
