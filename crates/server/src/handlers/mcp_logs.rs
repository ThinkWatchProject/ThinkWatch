use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize)]
pub struct McpLogsQuery {
    pub user_id: Option<String>,
    pub server_id: Option<String>,
    pub tool_name: Option<String>,
    pub status: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub sort_by: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row)]
pub struct McpLogEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub tool_name: Option<String>,
    pub duration_ms: Option<i64>,
    pub status: Option<String>,
    pub error_message: Option<String>,
    pub ip_address: Option<String>,
    #[serde(skip_deserializing)]
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct McpLogsResponse {
    pub items: Vec<McpLogEntry>,
    pub total: u64,
}

pub async fn list_mcp_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<McpLogsQuery>,
) -> Result<Json<McpLogsResponse>, AppError> {
    if !ch_available(&state) {
        return Ok(Json(McpLogsResponse { total: 0, items: vec![] }));
    }
    let ch = ch_client(&state)?;
    let limit = params.limit.unwrap_or(50).min(200);
    let offset = params.offset.unwrap_or(0);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = params.user_id { conditions.push("user_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.server_id { conditions.push("server_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.tool_name { conditions.push("tool_name = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.status { conditions.push("status = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.from { conditions.push("created_at >= ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.to { conditions.push("created_at <= ?".into()); binds.push(v.clone()); }

    let wc = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };
    let ob = match params.sort_by.as_deref() { Some("duration_ms") => "duration_ms DESC", _ => "created_at DESC" };

    let count_sql = format!("SELECT count() FROM mcp_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let total: u64 = q.fetch_one().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let data_sql = format!(
        "SELECT id, user_id, server_id, server_name, tool_name, duration_ms, status, error_message, ip_address, toString(created_at) as created_at \
         FROM mcp_logs {wc} ORDER BY {ob} LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let items: Vec<McpLogEntry> = q.fetch_all().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    Ok(Json(McpLogsResponse { total, items }))
}
