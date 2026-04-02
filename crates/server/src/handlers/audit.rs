use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::AuditLog;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Deserialize)]
pub struct AuditLogQuery {
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditLogResponse {
    pub items: Vec<AuditLog>,
    pub total: i64,
}

/// Escape SQL LIKE wildcards in user input.
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub async fn list_audit_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<AuditLogResponse>, AppError> {
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);

    // Validate and truncate search query
    let search_pattern = query.q.as_ref().map(|q| {
        let truncated = if q.len() > 255 { &q[..255] } else { q.as_str() };
        format!("%{}%", escape_like(truncated))
    });

    // Parse optional date range
    let from_dt = query.from.as_ref().and_then(|s| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
    });
    let to_dt = query.to.as_ref().and_then(|s| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .map(|d| d.and_hms_opt(23, 59, 59).unwrap())
    });

    // Build query dynamically but always use parameterized binds
    let has_search = search_pattern.is_some();
    let has_from = from_dt.is_some();
    let has_to = to_dt.is_some();

    // Construct WHERE clause parts
    let mut conditions = Vec::new();
    let mut param_idx = 1u32;

    if has_search {
        conditions.push(format!(
            "(action ILIKE ${p} ESCAPE '\\' OR resource ILIKE ${p} ESCAPE '\\')",
            p = param_idx
        ));
        param_idx += 1;
    }
    if has_from {
        conditions.push(format!("created_at >= ${p}::timestamptz", p = param_idx));
        param_idx += 1;
    }
    if has_to {
        conditions.push(format!("created_at <= ${p}::timestamptz", p = param_idx));
        param_idx += 1;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let items_sql = format!(
        "SELECT * FROM audit_logs {where_clause} ORDER BY created_at DESC LIMIT ${} OFFSET ${}",
        param_idx,
        param_idx + 1
    );
    let count_sql = format!("SELECT COUNT(*) FROM audit_logs {where_clause}");

    // Bind parameters in order
    let mut items_query = sqlx::query_as::<_, AuditLog>(&items_sql);
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);

    if let Some(ref pattern) = search_pattern {
        items_query = items_query.bind(pattern);
        count_query = count_query.bind(pattern);
    }
    if let Some(ref dt) = from_dt {
        items_query = items_query.bind(dt);
        count_query = count_query.bind(dt);
    }
    if let Some(ref dt) = to_dt {
        items_query = items_query.bind(dt);
        count_query = count_query.bind(dt);
    }

    items_query = items_query.bind(limit).bind(offset);

    let items = items_query.fetch_all(&state.db).await?;
    let total = count_query.fetch_one(&state.db).await?;

    Ok(Json(AuditLogResponse { items, total }))
}
