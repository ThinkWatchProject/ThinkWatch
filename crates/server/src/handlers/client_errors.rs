//! Endpoint for the frontend ErrorBoundary to POST client-side
//! crashes into. Records a structured `tracing::error!` line which
//! the ClickHouse layer ingests into `app_logs`, so operators can
//! query for browser-side incidents alongside server logs.
//!
//! Public (no auth): a logged-out user can hit this if the login
//! page itself crashes. Guarded by a small body-size limit so a
//! buggy client can't push megabytes through it.

use axum::Json;
use serde::Deserialize;
use think_watch_common::errors::AppError;

#[derive(Debug, Deserialize)]
pub struct ClientError {
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub component_stack: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
}

const MAX_FIELD_LEN: usize = 4_000;

fn truncate(s: Option<String>) -> Option<String> {
    s.map(|mut v| {
        if v.len() > MAX_FIELD_LEN {
            v.truncate(MAX_FIELD_LEN);
            v.push_str("…[truncated]");
        }
        v
    })
}

/// POST /api/client-errors — receive a frontend ErrorBoundary report.
pub async fn report_client_error(
    Json(report): Json<ClientError>,
) -> Result<Json<serde_json::Value>, AppError> {
    let report = ClientError {
        message: {
            let mut m = report.message;
            if m.len() > MAX_FIELD_LEN {
                m.truncate(MAX_FIELD_LEN);
                m.push_str("…[truncated]");
            }
            m
        },
        stack: truncate(report.stack),
        component_stack: truncate(report.component_stack),
        url: truncate(report.url),
        user_agent: truncate(report.user_agent),
        ts: report.ts,
    };
    tracing::error!(
        target: "client_error",
        message = %report.message,
        stack = report.stack.as_deref().unwrap_or(""),
        component_stack = report.component_stack.as_deref().unwrap_or(""),
        url = report.url.as_deref().unwrap_or(""),
        user_agent = report.user_agent.as_deref().unwrap_or(""),
        ts = report.ts.as_deref().unwrap_or(""),
        "Client-side ErrorBoundary fired"
    );
    Ok(Json(serde_json::json!({ "status": "received" })))
}
