use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::AuditLog;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize)]
pub struct AuditLogQuery {
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub ip_address: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditLogResponse {
    pub items: Vec<AuditLog>,
    pub total: u64,
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct ChAuditRow {
    id: String,
    user_id: Option<String>,
    user_email: Option<String>,
    api_key_id: Option<String>,
    action: String,
    resource: Option<String>,
    detail: Option<String>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    created_at: String,
}

pub async fn list_audit_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<AuditLogResponse>, AppError> {
    if !ch_available(&state) {
        return Ok(Json(AuditLogResponse { total: 0, items: vec![] }));
    }
    let ch = ch_client(&state)?;
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = query.user_id { conditions.push("user_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.api_key_id { conditions.push("api_key_id = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.action { conditions.push("action = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.resource { conditions.push("resource = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.ip_address { conditions.push("ip_address = ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.from { conditions.push("created_at >= ?".into()); binds.push(v.clone()); }
    if let Some(ref v) = query.to { conditions.push("created_at <= ?".into()); binds.push(v.clone()); }

    let wc = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };

    let count_sql = format!("SELECT count() FROM audit_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let total: u64 = q.fetch_one().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let data_sql = format!(
        "SELECT id, user_id, user_email, api_key_id, action, resource, detail, ip_address, user_agent, toString(created_at) as created_at \
         FROM audit_logs {wc} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds { q = q.bind(v.as_str()); }
    let rows: Vec<ChAuditRow> = q.fetch_all().await.map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let items: Vec<AuditLog> = rows
        .into_iter()
        .filter_map(|hit| {
            Some(AuditLog {
                id: hit.id.parse().ok()?,
                user_id: hit.user_id.and_then(|s| s.parse::<Uuid>().ok()),
                user_email: hit.user_email,
                api_key_id: hit.api_key_id.and_then(|s| s.parse::<Uuid>().ok()),
                action: hit.action,
                resource: hit.resource,
                detail: hit.detail.and_then(|s| serde_json::from_str(&s).ok()),
                ip_address: hit.ip_address,
                user_agent: hit.user_agent,
                created_at: chrono::DateTime::parse_from_str(&hit.created_at, "%Y-%m-%d %H:%M:%S%.f")
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .or_else(|_| chrono::DateTime::parse_from_rfc3339(&hit.created_at).map(|dt| dt.with_timezone(&chrono::Utc)))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
        })
        .collect();

    Ok(Json(AuditLogResponse { total, items }))
}
