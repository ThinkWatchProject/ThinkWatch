use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct AccessLogsQuery {
    pub method: Option<String>,
    pub path: Option<String>,
    pub status_code: Option<String>,
    pub port: Option<String>,
    pub user_id: Option<String>,
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Comma-separated `key:value` exclusions, e.g.
    /// `method:GET,status_code:200,path:/admin`. See
    /// [`super::clickhouse_util::parse_exclude_param`].
    pub exclude: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row, utoipa::ToSchema)]
pub struct AccessLogEntry {
    pub id: String,
    pub method: String,
    pub path: String,
    pub status_code: u16,
    pub latency_ms: i64,
    pub port: u16,
    pub user_id: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccessLogsResponse {
    pub items: Vec<AccessLogEntry>,
    pub total: u64,
}

#[utoipa::path(
    get,
    path = "/api/admin/access-logs",
    tag = "System Logs",
    params(AccessLogsQuery),
    responses(
        (status = 200, description = "Paginated HTTP access log entries", body = AccessLogsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_access_logs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<AccessLogsQuery>,
) -> Result<Json<AccessLogsResponse>, AppError> {
    auth_user.require_permission("logs:read_all")?;
    if !ch_available(&state) {
        return Ok(Json(AccessLogsResponse {
            total: 0,
            items: vec![],
        }));
    }
    let ch = ch_client(&state)?;
    let (limit, offset) = clamp_pagination(params.limit, params.offset, 200);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = params.method {
        conditions.push("method = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.path {
        conditions.push("path LIKE ?".into());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        binds.push(format!("%{escaped}%"));
    }
    if let Some(ref v) = params.status_code {
        conditions.push("status_code = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.port {
        conditions.push("port = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.user_id {
        conditions.push("user_id = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.q {
        conditions.push("path LIKE ?".into());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        binds.push(format!("%{escaped}%"));
    }
    if let Some(ref v) = params.from {
        conditions.push("created_at >= ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.to {
        conditions.push("created_at <= ?".into());
        binds.push(v.clone());
    }

    // Excludes (-key:value tokens from the unified log explorer)
    for (frag, val) in parse_exclude_param(
        params.exclude.as_deref(),
        &[
            ("method", "method", ExcludeMode::Equals),
            ("path", "path", ExcludeMode::NotLike),
            ("status_code", "status_code", ExcludeMode::Equals),
            ("port", "port", ExcludeMode::Equals),
            ("user_id", "user_id", ExcludeMode::Equals),
        ],
    ) {
        conditions.push(frag);
        binds.push(val);
    }

    let wc = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let count_sql = format!("SELECT count() FROM access_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let total: u64 = q
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let data_sql = format!(
        "SELECT id, method, path, status_code, latency_ms, port, user_id, ip_address, user_agent, \
         toString(created_at) as created_at \
         FROM access_logs {wc} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let items: Vec<AccessLogEntry> = q
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    Ok(Json(AccessLogsResponse { total, items }))
}
