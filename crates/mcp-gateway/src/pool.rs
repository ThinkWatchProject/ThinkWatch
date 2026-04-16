use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::proxy::{JsonRpcRequest, JsonRpcResponse};
use crate::registry::RegisteredServer;

/// Caller identity passed per-request for template header resolution.
#[derive(Debug, Clone)]
pub struct CallerIdentity {
    pub user_id: String,
    pub user_email: String,
}

/// A single connection to an upstream MCP server.
///
/// Intentionally **stateless** with respect to sessions — the upstream
/// `Mcp-Session-Id` is managed per-user by [`crate::session::SessionManager`]
/// and passed into [`ConnectionPool::send_request`] on every call.  This
/// makes it safe to share one connection across all users hitting the
/// same upstream server.
#[derive(Debug, Clone)]
pub struct McpConnection {
    pub server_id: Uuid,
    pub endpoint_url: String,
    client: Client,
    /// `(header name, header value)` to attach to every upstream request.
    /// Resolved at connection creation from `RegisteredServer.auth_header`.
    auth_header: Option<(String, String)>,
    /// Custom headers with optional template variables (`{{user_id}}`,
    /// `{{user_email}}`), resolved per-request.
    custom_headers: Vec<(String, String)>,
}

impl McpConnection {
    fn new(server: &RegisteredServer, client: Client) -> Self {
        Self {
            server_id: server.id,
            endpoint_url: server.endpoint_url.clone(),
            client,
            auth_header: server.auth_header.clone(),
            custom_headers: server.custom_headers.clone(),
        }
    }
}

/// Error type for connection-pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("HTTP request to upstream MCP server failed: {0}")]
    RequestFailed(#[from] reqwest::Error),

    #[error("Upstream MCP server returned non-success status {status}: {body}")]
    UpstreamError { status: u16, body: String },

    #[error("Failed to parse upstream JSON-RPC response: {0}")]
    ParseError(String),
}

/// Manages a pool of `McpConnection`s keyed by server ID.
#[derive(Clone)]
pub struct ConnectionPool {
    connections: Arc<RwLock<HashMap<Uuid, McpConnection>>>,
    client: Client,
}

impl ConnectionPool {
    /// Create a pool with the default 30s per-request timeout.
    pub fn new() -> Self {
        Self::with_timeout(30)
    }

    /// Create a pool with a custom per-request timeout (in seconds).
    /// Used by the server crate to wire `Timeouts.mcp_pool_secs` through
    /// from `AppConfig` so the timeout is operator-tunable.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            client,
        }
    }

    /// Return an existing connection for the server, or create a new one.
    pub async fn get_or_create(&self, server: &RegisteredServer) -> McpConnection {
        // Fast path: read lock.
        {
            let conns = self.connections.read().await;
            if let Some(conn) = conns.get(&server.id) {
                return conn.clone();
            }
        }

        // Slow path: write lock.
        let mut conns = self.connections.write().await;
        // Double-check after acquiring write lock.
        if let Some(conn) = conns.get(&server.id) {
            return conn.clone();
        }

        let conn = McpConnection::new(server, self.client.clone());
        conns.insert(server.id, conn.clone());
        conn
    }

    /// Remove the cached connection for a server (e.g. after a health-check
    /// failure or server deregistration).
    pub async fn remove(&self, server_id: Uuid) {
        let mut conns = self.connections.write().await;
        conns.remove(&server_id);
    }

    /// Send a JSON-RPC request to an upstream MCP server and return the
    /// parsed response together with any upstream session ID the server
    /// sent back.  The caller is responsible for persisting the returned
    /// session ID (typically via [`crate::session::SessionManager`]) and
    /// passing it back on the next call.
    pub async fn send_request(
        &self,
        conn: &McpConnection,
        request: &JsonRpcRequest,
        caller: Option<&CallerIdentity>,
        upstream_session_id: Option<&str>,
    ) -> Result<(JsonRpcResponse, Option<String>), PoolError> {
        let mut builder = conn
            .client
            .post(&conn.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        // Attach upstream auth header if the server has one configured.
        if let Some((name, value)) = &conn.auth_header {
            builder = builder.header(name.as_str(), value.as_str());
        }

        // Attach custom headers with template variable resolution.
        // Values containing {{user_id}} or {{user_email}} are resolved
        // per-request; plain values pass through as-is.
        for (key, template) in &conn.custom_headers {
            let value = if let Some(c) = caller {
                template
                    .replace("{{user_id}}", &c.user_id)
                    .replace("{{user_email}}", &c.user_email)
            } else {
                template.clone()
            };
            builder = builder.header(key.as_str(), value);
        }

        // Attach the caller-supplied upstream session header, if any.
        if let Some(sid) = upstream_session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }

        let resp = builder.json(request).send().await?;

        // Capture the upstream session ID from the response header so the
        // caller can persist it per-user.
        let new_session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(PoolError::UpstreamError {
                status: status.as_u16(),
                body,
            });
        }

        // The MCP Streamable HTTP spec allows the server to reply with
        // either `application/json` (plain JSON-RPC) or `text/event-stream`
        // (SSE wrapping a JSON-RPC message in a `data:` line).  We must
        // handle both.
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let json_resp: JsonRpcResponse = if content_type.contains("text/event-stream") {
            let text = resp
                .text()
                .await
                .map_err(|e| PoolError::ParseError(e.to_string()))?;
            parse_sse_json_rpc(&text)?
        } else {
            resp.json()
                .await
                .map_err(|e| PoolError::ParseError(e.to_string()))?
        };

        // Validate JSON-RPC version
        if json_resp.jsonrpc != "2.0" {
            return Err(PoolError::ParseError("Invalid JSON-RPC version".into()));
        }

        Ok((json_resp, new_session_id))
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the first JSON-RPC response from an SSE body.
///
/// SSE format is:
/// ```text
/// event: message
/// data: {"jsonrpc":"2.0", ...}
/// ```
///
/// We scan for `data:` lines and try to parse each as JSON-RPC until one
/// succeeds.  Multi-line `data:` fields are concatenated per the SSE spec.
fn parse_sse_json_rpc(text: &str) -> Result<JsonRpcResponse, PoolError> {
    let mut data_buf = String::new();

    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim_start();
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(payload);
        } else if line.is_empty() && !data_buf.is_empty() {
            // End of an SSE event — try to parse what we have.
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&data_buf) {
                return Ok(resp);
            }
            data_buf.clear();
        }
    }

    // Handle case where stream ends without a trailing blank line.
    if !data_buf.is_empty()
        && let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&data_buf)
    {
        return Ok(resp);
    }

    Err(PoolError::ParseError(
        "No valid JSON-RPC response found in SSE stream".into(),
    ))
}
