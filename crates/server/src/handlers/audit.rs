use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::AuditLog;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

fn escape_query_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if "+-&|!(){}[]^\"~*?:\\/ ".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn parse_date_start(s: &str) -> Result<i64, AppError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|dt| dt.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
        .map_err(|_| {
            AppError::BadRequest(format!("Invalid date format '{}', expected YYYY-MM-DD", s))
        })
}

fn parse_date_end(s: &str) -> Result<i64, AppError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|dt| dt.and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp())
        .map_err(|_| {
            AppError::BadRequest(format!("Invalid date format '{}', expected YYYY-MM-DD", s))
        })
}

#[derive(Debug, Deserialize)]
pub struct AuditLogQuery {
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub ip_address: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditLogResponse {
    pub items: Vec<AuditLog>,
    pub total: i64,
}

pub async fn list_audit_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<AuditLogResponse>, AppError> {
    let qw_url = state
        .config
        .quickwit_url
        .as_deref()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Quickwit is not configured")))?;
    query_quickwit(
        qw_url,
        &state.config.quickwit_index,
        state.config.quickwit_bearer_token.as_deref(),
        &query,
    )
    .await
}

// ---------------------------------------------------------------------------
// Quickwit search
// ---------------------------------------------------------------------------

/// Quickwit search API response shape.
#[derive(Debug, Deserialize)]
struct QwSearchResponse {
    num_hits: i64,
    hits: Vec<QwHit>,
}

#[derive(Debug, Deserialize)]
struct QwHit {
    id: String,
    user_id: Option<String>,
    #[allow(dead_code)]
    api_key_id: Option<String>,
    action: String,
    resource: Option<String>,
    detail: Option<serde_json::Value>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    created_at: String,
}

async fn query_quickwit(
    qw_url: &str,
    qw_index: &str,
    bearer_token: Option<&str>,
    query: &AuditLogQuery,
) -> Result<Json<AuditLogResponse>, AppError> {
    let max_hits = query.limit.unwrap_or(50).min(200);
    let start_offset = query.offset.unwrap_or(0);

    // Build Quickwit query string with structured filters
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref q) = query.q {
        let q = if q.len() > 255 { &q[..255] } else { q.as_str() };
        if !q.is_empty() && q != "*" {
            parts.push(q.to_string());
        }
    }
    if let Some(ref v) = query.user_id {
        parts.push(format!("user_id:{}", escape_query_value(v)));
    }
    if let Some(ref v) = query.api_key_id {
        parts.push(format!("api_key_id:{}", escape_query_value(v)));
    }
    if let Some(ref v) = query.action {
        parts.push(format!("action:{}", escape_query_value(v)));
    }
    if let Some(ref v) = query.resource {
        parts.push(format!("resource:{}", escape_query_value(v)));
    }
    if let Some(ref v) = query.ip_address {
        parts.push(format!("ip_address:{}", escape_query_value(v)));
    }

    let search_query = if parts.is_empty() {
        "*".to_string()
    } else {
        parts.join(" AND ")
    };

    let mut url = format!(
        "{}/api/v1/{}/search?query={}&max_hits={}&start_offset={}&sort_by_field=-created_at",
        qw_url,
        qw_index,
        urlencoding::encode(&search_query),
        max_hits,
        start_offset,
    );

    // Optional timestamp range filters
    if let Some(ref from) = query.from {
        let epoch = parse_date_start(from)?;
        url.push_str(&format!("&start_timestamp={epoch}"));
    }
    if let Some(ref to) = query.to {
        let epoch = parse_date_end(to)?;
        url.push_str(&format!("&end_timestamp={epoch}"));
    }

    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if let Some(token) = bearer_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Quickwit search request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(anyhow::anyhow!(
            "Quickwit search returned {status}: {body}"
        )));
    }

    let qw_resp = resp.json::<QwSearchResponse>().await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("Failed to parse Quickwit response: {e}"))
    })?;

    let items: Vec<AuditLog> = qw_resp
        .hits
        .into_iter()
        .filter_map(|hit| {
            Some(AuditLog {
                id: hit.id.parse().ok()?,
                user_id: hit.user_id.and_then(|s| s.parse::<Uuid>().ok()),
                api_key_id: hit.api_key_id.and_then(|s| s.parse::<Uuid>().ok()),
                action: hit.action,
                resource: hit.resource,
                detail: hit.detail,
                ip_address: hit.ip_address,
                user_agent: hit.user_agent,
                created_at: chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
        })
        .collect();

    Ok(Json(AuditLogResponse {
        total: qw_resp.num_hits,
        items,
    }))
}
