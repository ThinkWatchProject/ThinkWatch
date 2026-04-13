use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Information about a single tool exposed by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

/// Transport type used to connect to an upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportType {
    StreamableHttp,
}

/// Runtime status of a registered MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    Connected,
    Disconnected,
    Unknown,
}

/// A server that has been registered with the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredServer {
    pub id: Uuid,
    pub name: String,
    pub endpoint_url: String,
    pub transport_type: TransportType,
    pub tools: Vec<McpToolInfo>,
    pub status: ServerStatus,
    pub last_health_check: Option<DateTime<Utc>>,
    /// Optional `(header name, header value)` to attach to every upstream
    /// request. Resolved once at registration time from the encrypted
    /// `auth_secret` column. Skipped from serialization to avoid leaking
    /// the secret through any debug/log surface.
    #[serde(skip)]
    pub auth_header: Option<(String, String)>,
    /// Custom headers attached to every upstream request. Values may
    /// contain `{{user_id}}` and `{{user_email}}` template variables
    /// which are resolved per-request from the caller's identity.
    #[serde(skip)]
    pub custom_headers: Vec<(String, String)>,
}

/// The namespace separator used to prefix tool names with their server name.
pub const NAMESPACE_SEPARATOR: &str = "__";

/// Thread-safe registry of upstream MCP servers.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<Uuid, RegisteredServer>>>,
}

/// Validate that a tool name contains only safe characters.
fn validate_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register (or update) a server in the registry. Filters out tools with invalid names.
    pub async fn register(&self, mut server: RegisteredServer) {
        server.tools.retain(|t| validate_tool_name(&t.name));
        let mut servers = self.inner.write().await;
        servers.insert(server.id, server);
    }

    /// Remove a server from the registry.
    pub async fn unregister(&self, id: Uuid) {
        let mut servers = self.inner.write().await;
        servers.remove(&id);
    }

    /// Look up a server by its ID.
    pub async fn get(&self, id: Uuid) -> Option<RegisteredServer> {
        let servers = self.inner.read().await;
        servers.get(&id).cloned()
    }

    /// Return all registered servers.
    pub async fn list(&self) -> Vec<RegisteredServer> {
        let servers = self.inner.read().await;
        servers.values().cloned().collect()
    }

    /// Update the status and last health-check timestamp for a server.
    pub async fn update_status(&self, id: Uuid, status: ServerStatus) {
        let mut servers = self.inner.write().await;
        if let Some(server) = servers.get_mut(&id) {
            server.status = status;
            server.last_health_check = Some(Utc::now());
        }
    }

    /// Update the cached tool list for a server. Filters out tools with invalid names.
    pub async fn update_tools(&self, id: Uuid, tools: Vec<McpToolInfo>) {
        let mut servers = self.inner.write().await;
        if let Some(server) = servers.get_mut(&id) {
            server.tools = tools
                .into_iter()
                .filter(|t| validate_tool_name(&t.name))
                .collect();
        }
    }

    /// Given a namespaced tool name (`server_name__tool_name`), find the
    /// owning server and return it together with the original (un-prefixed)
    /// tool name.
    pub async fn find_server_for_tool(
        &self,
        namespaced_tool: &str,
    ) -> Option<(RegisteredServer, String)> {
        let (server_name, tool_name) = namespaced_tool.split_once(NAMESPACE_SEPARATOR)?;
        if server_name.is_empty() || tool_name.is_empty() {
            return None;
        }

        let servers = self.inner.read().await;
        let server = servers.values().find(|s| s.name == server_name)?.clone();

        Some((server, tool_name.to_owned()))
    }

    /// Collect all tools from all (or a subset of) registered servers,
    /// returning them with their namespaced name.
    pub async fn get_all_tools(
        &self,
        filter_server_ids: Option<&[Uuid]>,
    ) -> Vec<(String, McpToolInfo)> {
        let servers = self.inner.read().await;
        let mut result = Vec::new();

        for server in servers.values() {
            if let Some(ids) = filter_server_ids
                && !ids.contains(&server.id)
            {
                continue;
            }
            for tool in &server.tools {
                let namespaced = format!("{}{NAMESPACE_SEPARATOR}{}", server.name, tool.name);
                result.push((namespaced, tool.clone()));
            }
        }

        result
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server(name: &str, tools: Vec<&str>) -> RegisteredServer {
        RegisteredServer {
            id: Uuid::new_v4(),
            name: name.to_string(),
            endpoint_url: format!("http://{name}.local/mcp"),
            transport_type: TransportType::StreamableHttp,
            tools: tools
                .into_iter()
                .map(|t| McpToolInfo {
                    name: t.to_string(),
                    description: None,
                    input_schema: None,
                })
                .collect(),
            status: ServerStatus::Connected,
            last_health_check: None,
            auth_header: None,
            custom_headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn register_and_get() {
        let reg = Registry::new();
        let server = make_server("github", vec!["list_issues"]);
        let id = server.id;

        reg.register(server).await;

        let retrieved = reg.get(id).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "github");
    }

    #[tokio::test]
    async fn find_server_for_tool_with_namespace() {
        let reg = Registry::new();
        let server = make_server("slack", vec!["send_message"]);
        reg.register(server).await;

        let result = reg.find_server_for_tool("slack__send_message").await;
        assert!(result.is_some());
        let (srv, tool_name) = result.unwrap();
        assert_eq!(srv.name, "slack");
        assert_eq!(tool_name, "send_message");
    }

    #[tokio::test]
    async fn find_server_for_tool_no_separator_returns_none() {
        let reg = Registry::new();
        let result = reg.find_server_for_tool("no_namespace_here").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_server_for_tool_empty_parts_returns_none() {
        let reg = Registry::new();
        let result = reg.find_server_for_tool("__tool").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_all_tools_aggregation() {
        let reg = Registry::new();
        let s1 = make_server("github", vec!["list_issues", "create_pr"]);
        let s2 = make_server("slack", vec!["send_message"]);
        let s1_id = s1.id;

        reg.register(s1).await;
        reg.register(s2).await;

        // No filter: all tools
        let all = reg.get_all_tools(None).await;
        assert_eq!(all.len(), 3);

        // Filter to s1 only
        let filtered = reg.get_all_tools(Some(&[s1_id])).await;
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .all(|(name, _)| name.starts_with("github__"))
        );
    }
}
