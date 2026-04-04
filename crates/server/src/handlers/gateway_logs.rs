use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Escape special Tantivy/Quickwit query syntax characters in a field value.
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
pub struct GatewayLogsQuery {
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

#[derive(Debug, Serialize, Deserialize)]
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
    pub total: i64,
}

pub async fn list_gateway_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<GatewayLogsQuery>,
) -> Result<Json<GatewayLogsResponse>, AppError> {
    let qw_url = state
        .config
        .quickwit_url
        .as_deref()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Quickwit is not configured")))?;

    let max_hits = params.limit.unwrap_or(50).min(200);
    let start_offset = params.offset.unwrap_or(0);

    // Build structured query
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref q) = params.q {
        let q = if q.len() > 255 { &q[..255] } else { q.as_str() };
        if !q.is_empty() && q != "*" {
            parts.push(q.to_string());
        }
    }
    if let Some(ref v) = params.model {
        parts.push(format!("model_id:{}", escape_query_value(v)));
    }
    if let Some(ref v) = params.provider {
        parts.push(format!("provider:{}", escape_query_value(v)));
    }
    if let Some(ref v) = params.user_id {
        parts.push(format!("user_id:{}", escape_query_value(v)));
    }
    if let Some(ref v) = params.api_key_id {
        parts.push(format!("api_key_id:{}", escape_query_value(v)));
    }
    if let Some(v) = params.status_code {
        parts.push(format!("status_code:{v}"));
    }

    let search_query = if parts.is_empty() {
        "*".to_string()
    } else {
        parts.join(" AND ")
    };

    let sort_field = match params.sort_by.as_deref() {
        Some("cost_usd") => "-cost_usd",
        Some("latency_ms") => "-latency_ms",
        _ => "-created_at",
    };

    let mut url = format!(
        "{}/api/v1/gateway_logs/search?query={}&max_hits={}&start_offset={}&sort_by_field={}",
        qw_url,
        urlencoding::encode(&search_query),
        max_hits,
        start_offset,
        sort_field,
    );

    if let Some(ref from) = params.from {
        let epoch = parse_date_start(from)?;
        url.push_str(&format!("&start_timestamp={epoch}"));
    }
    if let Some(ref to) = params.to {
        let epoch = parse_date_end(to)?;
        url.push_str(&format!("&end_timestamp={epoch}"));
    }

    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if let Some(ref token) = state.config.quickwit_bearer_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Quickwit search failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(anyhow::anyhow!(
            "Quickwit search returned {status}: {body}"
        )));
    }

    #[derive(Deserialize)]
    struct QwResp {
        num_hits: i64,
        hits: Vec<GatewayLogEntry>,
    }

    let qw_resp = resp.json::<QwResp>().await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("Failed to parse Quickwit response: {e}"))
    })?;

    Ok(Json(GatewayLogsResponse {
        total: qw_resp.num_hits,
        items: qw_resp.hits,
    }))
}
