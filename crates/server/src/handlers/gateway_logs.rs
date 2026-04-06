use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize)]
pub struct GatewayLogsQuery {
    /// Free-text search (reserved for future ngramSearch implementation)
    #[allow(dead_code)]
    pub q: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub status_code: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub sort_by: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row)]
pub struct GatewayLogEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub ip_address: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct GatewayLogsResponse {
    pub items: Vec<GatewayLogEntry>,
    pub total: u64,
}

pub async fn list_gateway_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<GatewayLogsQuery>,
) -> Result<Json<GatewayLogsResponse>, AppError> {
    if !ch_available(&state) {
        return Ok(Json(GatewayLogsResponse {
            total: 0,
            items: vec![],
        }));
    }
    let ch = ch_client(&state)?;
    let limit = params.limit.unwrap_or(50).min(200);
    let offset = params.offset.unwrap_or(0);

    // Build dynamic WHERE conditions
    let mut conditions: Vec<String> = Vec::new();
    let mut bind_values: Vec<String> = Vec::new();

    // ClickHouse SDK uses ? for bind params in order
    if let Some(ref v) = params.model {
        conditions.push("model_id = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(ref v) = params.provider {
        conditions.push("provider = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(ref v) = params.user_id {
        conditions.push("user_id = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(ref v) = params.api_key_id {
        conditions.push("api_key_id = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(v) = params.status_code {
        conditions.push("status_code = ?".to_string());
        bind_values.push(v.to_string());
    }
    if let Some(ref from) = params.from {
        conditions.push("created_at >= ?".to_string());
        bind_values.push(from.clone());
    }
    if let Some(ref to) = params.to {
        conditions.push("created_at <= ?".to_string());
        bind_values.push(to.clone());
    }
    // Free-text `q` searches model_id with case-insensitive substring match.
    // The user input is escaped for LIKE wildcards (% / _ / \) so they can
    // only match literal characters, not patterns.
    if let Some(ref v) = params.q
        && !v.is_empty()
    {
        conditions.push("model_id LIKE ?".to_string());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        bind_values.push(format!("%{escaped}%"));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let order_by = match params.sort_by.as_deref() {
        Some("cost_usd") => "cost_usd DESC",
        Some("latency_ms") => "latency_ms DESC",
        _ => "created_at DESC",
    };

    // Count query
    let count_sql = format!("SELECT count() FROM gateway_logs {where_clause}");
    let mut count_query = ch.query(&count_sql);
    for v in &bind_values {
        count_query = count_query.bind(v.as_str());
    }
    let total: u64 = count_query
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse query failed: {e}")))?;

    // Data query
    let data_sql = format!(
        "SELECT id, user_id, api_key_id, model_id, provider, input_tokens, output_tokens, \
         cost_usd, latency_ms, status_code, ip_address, \
         toString(created_at) as created_at \
         FROM gateway_logs {where_clause} ORDER BY {order_by} LIMIT {limit} OFFSET {offset}"
    );
    let mut data_query = ch.query(&data_sql);
    for v in &bind_values {
        data_query = data_query.bind(v.as_str());
    }
    let items: Vec<GatewayLogEntry> = data_query
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse query failed: {e}")))?;

    Ok(Json(GatewayLogsResponse { total, items }))
}
