use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::proxy::{JsonRpcRequest, JsonRpcResponse};
use crate::registry::RegisteredServer;

/// A single connection to an upstream MCP server.
#[derive(Debug, Clone)]
pub struct McpConnection {
    pub server_id: Uuid,
    pub endpoint_url: String,
    client: Client,
    /// The `Mcp-Session-Id` header value returned by the upstream server
    /// during initialization, if any.
    session_id: Arc<RwLock<Option<String>>>,
    /// `(header name, header value)` to attach to every upstream request.
    /// Resolved at connection creation from `RegisteredServer.auth_header`.
    auth_header: Option<(String, String)>,
}

impl McpConnection {
    fn new(server: &RegisteredServer, client: Client) -> Self {
        Self {
            server_id: server.id,
            endpoint_url: server.endpoint_url.clone(),
            client,
            session_id: Arc::new(RwLock::new(None)),
            auth_header: server.auth_header.clone(),
        }
    }

    /// Store the upstream session ID (typically received in a response header).
    pub async fn set_session_id(&self, id: String) {
        let mut sid = self.session_id.write().await;
        *sid = Some(id);
    }

    /// Retrieve the current upstream session ID, if one has been established.
    pub async fn get_session_id(&self) -> Option<String> {
        self.session_id.read().await.clone()
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
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
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
    /// parsed response.
    pub async fn send_request(
        &self,
        conn: &McpConnection,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, PoolError> {
        let mut builder = conn
            .client
            .post(&conn.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");

        // Attach upstream auth header if the server has one configured.
        if let Some((name, value)) = &conn.auth_header {
            builder = builder.header(name.as_str(), value.as_str());
        }

        // Attach upstream session header if we have one.
        if let Some(sid) = conn.get_session_id().await {
            builder = builder.header("Mcp-Session-Id", &sid);
        }

        let resp = builder.json(request).send().await?;

        // Capture the upstream session ID from the response header.
        if let Some(sid) = resp.headers().get("mcp-session-id")
            && let Ok(s) = sid.to_str()
        {
            conn.set_session_id(s.to_owned()).await;
        }

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(PoolError::UpstreamError {
                status: status.as_u16(),
                body,
            });
        }

        let json_resp: JsonRpcResponse = resp
            .json()
            .await
            .map_err(|e| PoolError::ParseError(e.to_string()))?;

        // Validate JSON-RPC version
        if json_resp.jsonrpc != "2.0" {
            return Err(PoolError::ParseError("Invalid JSON-RPC version".into()));
        }

        Ok(json_resp)
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}
