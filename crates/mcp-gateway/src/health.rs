use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pool::ConnectionPool;
use crate::proxy::JsonRpcRequest;
use crate::registry::{RegisteredServer, Registry, ServerStatus};

/// Result of a single health check against an upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHealth {
    pub status: String,
    pub latency_ms: Option<u64>,
    pub last_check: DateTime<Utc>,
    pub error: Option<String>,
}

/// Performs health checks on registered MCP servers by sending an
/// `initialize` JSON-RPC request and measuring the round-trip time.
#[derive(Clone)]
pub struct HealthChecker {
    pool: ConnectionPool,
}

impl HealthChecker {
    pub fn new(pool: ConnectionPool) -> Self {
        Self { pool }
    }

    /// Probe a single server and return its health status.
    pub async fn check_server(&self, server: &RegisteredServer) -> ServerHealth {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Some(serde_json::json!("health-check")),
            method: "initialize".to_owned(),
            params: Some(serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "ThinkWatch HealthChecker",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
        };

        let conn = self.pool.get_or_create(server).await;
        let start = tokio::time::Instant::now();
        // Health checks are system-level — no user session, no caller identity.
        let result = self.pool.send_request(&conn, &request, None, None).await;
        let elapsed = start.elapsed();

        match result {
            Ok((resp, _upstream_sid)) if resp.error.is_none() => ServerHealth {
                status: "healthy".to_owned(),
                latency_ms: Some(elapsed.as_millis() as u64),
                last_check: Utc::now(),
                error: None,
            },
            Ok((resp, _)) => {
                let msg = resp
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "unknown error".to_owned());
                ServerHealth {
                    status: "unhealthy".to_owned(),
                    latency_ms: Some(elapsed.as_millis() as u64),
                    last_check: Utc::now(),
                    error: Some(msg),
                }
            }
            Err(e) => ServerHealth {
                status: "unhealthy".to_owned(),
                latency_ms: Some(elapsed.as_millis() as u64),
                last_check: Utc::now(),
                error: Some(e.to_string()),
            },
        }
    }

    /// Spawn a background tokio task that periodically health-checks every
    /// registered server and updates its status in the registry.
    pub fn start_background_checks(self, registry: Registry, interval_secs: u64) {
        let interval = Duration::from_secs(interval_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;

                let servers = registry.list().await;
                for server in &servers {
                    let health = self.check_server(server).await;
                    let new_status = match health.status.as_str() {
                        "healthy" => ServerStatus::Connected,
                        _ => ServerStatus::Disconnected,
                    };
                    registry.update_status(server.id, new_status).await;
                    tracing::debug!(
                        server_id = %server.id,
                        server_name = %server.name,
                        status = %health.status,
                        latency_ms = ?health.latency_ms,
                        "health check completed"
                    );
                }
            }
        });
    }
}
