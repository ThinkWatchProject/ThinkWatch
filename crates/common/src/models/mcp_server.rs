use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct McpServer {
    pub id: Uuid,
    pub name: String,
    pub namespace_prefix: String,
    pub description: Option<String>,
    pub endpoint_url: String,
    pub transport_type: String,
    pub auth_type: Option<String>,
    #[serde(skip_serializing)]
    pub auth_secret_encrypted: Option<Vec<u8>>,
    pub status: String,
    pub health_check_interval: Option<i32>,
    pub last_health_check: Option<DateTime<Utc>>,
    /// Most recent failure message from tool discovery / health check.
    /// `NULL` means the last attempt succeeded. Surfaced in the admin UI
    /// so operators don't have to dig through server logs.
    #[serde(default)]
    pub last_error: Option<String>,
    pub config_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// Computed field: number of active tools discovered for this server.
    #[sqlx(default)]
    #[serde(default)]
    pub tools_count: i64,
    /// Computed field: lifetime call count (from ClickHouse mcp_logs).
    /// 0 when ClickHouse is unavailable.
    #[sqlx(default)]
    #[serde(default)]
    pub call_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct McpTool {
    pub id: Uuid,
    pub server_id: Uuid,
    pub tool_name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
    pub is_active: bool,
    pub discovered_at: DateTime<Utc>,
}
