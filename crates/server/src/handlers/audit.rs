use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

pub async fn list_audit_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<AuditLogResponse>, AppError> {
    let qw_url = state.config.quickwit_url.as_deref().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("Quickwit is not configured"))
    })?;
    query_quickwit(qw_url, &state.config.quickwit_index, &query).await
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
    query: &AuditLogQuery,
) -> Result<Json<AuditLogResponse>, AppError> {
    let max_hits = query.limit.unwrap_or(50).min(200);
    let start_offset = query.offset.unwrap_or(0);

    // Build Quickwit query string
    let search_query = query.q.as_deref().unwrap_or("*");
    // Truncate to prevent oversized queries
    let search_query = if search_query.len() > 255 {
        &search_query[..255]
    } else {
        search_query
    };

    let mut url = format!(
        "{}/api/v1/{}/search?query={}&max_hits={}&start_offset={}&sort_by_field=-created_at",
        qw_url,
        qw_index,
        urlencoding::encode(search_query),
        max_hits,
        start_offset,
    );

    // Optional timestamp range filters
    if let Some(ref from) = query.from {
        if let Ok(dt) = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d") {
            let ts = dt.and_hms_opt(0, 0, 0).unwrap();
            let epoch = ts.and_utc().timestamp();
            url.push_str(&format!("&start_timestamp={epoch}"));
        }
    }
    if let Some(ref to) = query.to {
        if let Ok(dt) = chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d") {
            let ts = dt.and_hms_opt(23, 59, 59).unwrap();
            let epoch = ts.and_utc().timestamp();
            url.push_str(&format!("&end_timestamp={epoch}"));
        }
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
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

    let qw_resp = resp
        .json::<QwSearchResponse>()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to parse Quickwit response: {e}")))?;

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
