use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize)]
pub struct PlatformLogsQuery {
    pub user_id: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub resource_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row)]
pub struct PlatformLogEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub resource_id: Option<String>,
    /// detail is stored as a String in ClickHouse; deserialize to Value in post-processing
    pub detail: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    #[serde(skip_deserializing)]
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct PlatformLogEntryResponse {
    pub id: String,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub resource_id: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct PlatformLogsResponse {
    pub items: Vec<PlatformLogEntryResponse>,
    pub total: u64,
}

pub async fn list_platform_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<PlatformLogsQuery>,
) -> Result<Json<PlatformLogsResponse>, AppError> {
    if !ch_available(&state) {
        return Ok(Json(PlatformLogsResponse { total: 0, items: vec![] }));
    }
    let ch = ch_client(&state)?;
    let limit = params.limit.unwrap_or(50).min(200);
    let offset = params.offset.unwrap_or(0);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = params.user_id { conditions.push("user_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.action { conditions.push("action = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.resource { conditions.push("resource = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.resource_id { conditions.push("resource_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.from { conditions.push("created_at >= ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = params.to { conditions.push("created_at <= ?".into()); binds.push(v.clone()); }

    let wc = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };

    let count_sql = format!("SELECT count() FROM platform_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let total: u64 = q.fetch_one().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let data_sql = format!(
        "SELECT id, user_id, user_email, action, resource, resource_id, detail, ip_address, user_agent, toString(created_at) as created_at \
         FROM platform_logs {wc} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let rows: Vec<PlatformLogEntry> = q.fetch_all().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let items = rows
        .into_iter()
        .map(|r| PlatformLogEntryResponse {
            id: r.id,
            user_id: r.user_id,
            user_email: r.user_email,
            action: r.action,
            resource: r.resource,
            resource_id: r.resource_id,
            detail: r.detail.and_then(|s| serde_json::from_str(&s).ok()),
            ip_address: r.ip_address,
            user_agent: r.user_agent,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(PlatformLogsResponse { total, items }))
}
