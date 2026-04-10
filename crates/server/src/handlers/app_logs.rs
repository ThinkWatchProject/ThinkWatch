use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct AppLogsQuery {
    pub level: Option<String>,
    pub target: Option<String>,
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub exclude: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row)]
pub struct AppLogEntry {
    pub id: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Option<String>,
    pub span: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AppLogEntryResponse {
    pub id: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Option<serde_json::Value>,
    pub span: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AppLogsResponse {
    pub items: Vec<AppLogEntryResponse>,
    pub total: u64,
}

#[utoipa::path(
    get,
    path = "/api/admin/app-logs",
    tag = "System Logs",
    params(AppLogsQuery),
    responses(
        (status = 200, description = "Paginated application runtime log entries", body = AppLogsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_app_logs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<AppLogsQuery>,
) -> Result<Json<AppLogsResponse>, AppError> {
    auth_user.require_permission("logs:read_all")?;
    if !ch_available(&state) {
        return Ok(Json(AppLogsResponse {
            total: 0,
            items: vec![],
        }));
    }
    let ch = ch_client(&state)?;
    let (limit, offset) = clamp_pagination(params.limit, params.offset, 200);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = params.level {
        conditions.push("level = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.target {
        conditions.push("target LIKE ?".into());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        binds.push(format!("%{escaped}%"));
    }
    if let Some(ref v) = params.q {
        conditions.push("message LIKE ?".into());
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

    for (frag, val) in parse_exclude_param(
        params.exclude.as_deref(),
        &[
            ("level", "level", ExcludeMode::Equals),
            ("target", "target", ExcludeMode::NotLike),
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

    let count_sql = format!("SELECT count() FROM app_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let total: u64 = q
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let data_sql = format!(
        "SELECT id, level, target, message, fields, span, toString(created_at) as created_at \
         FROM app_logs {wc} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let rows: Vec<AppLogEntry> = q
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let items = rows
        .into_iter()
        .map(|r| AppLogEntryResponse {
            id: r.id,
            level: r.level,
            target: r.target,
            message: r.message,
            fields: r.fields.and_then(|s| serde_json::from_str(&s).ok()),
            span: r.span,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(AppLogsResponse { total, items }))
}
