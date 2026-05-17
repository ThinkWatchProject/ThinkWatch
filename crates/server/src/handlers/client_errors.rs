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

fn truncate_inplace(v: &mut String) {
    if v.len() > MAX_FIELD_LEN {
        // `String::truncate` panics if the cut isn't on a char
        // boundary — for a UTF-8 string whose Nth byte falls
        // mid-codepoint we'd crash the server on an inbound report.
        // Walk back to the nearest boundary at or below MAX_FIELD_LEN.
        let mut cut = MAX_FIELD_LEN;
        while !v.is_char_boundary(cut) {
            cut -= 1;
        }
        v.truncate(cut);
        v.push_str("…[truncated]");
    }
}

fn truncate(s: Option<String>) -> Option<String> {
    s.map(|mut v| {
        truncate_inplace(&mut v);
        v
    })
}

/// POST /api/client-errors — receive a frontend ErrorBoundary report.
pub async fn report_client_error(
    Json(report): Json<ClientError>,
) -> Result<Json<serde_json::Value>, AppError> {
    let report = ClientError {
        message: {
            // Was: `m.truncate(MAX_FIELD_LEN)` direct — that panics
            // if byte MAX_FIELD_LEN falls mid-codepoint, which the
            // sibling `truncate()` helper already handles via
            // char-boundary walk-back. Reuse it so a Chinese / emoji
            // crash message can't take the server down with a 500.
            let mut m = report.message;
            truncate_inplace(&mut m);
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

#[cfg(test)]
mod tests {
    use super::{MAX_FIELD_LEN, truncate, truncate_inplace};

    #[test]
    fn truncate_inplace_handles_mid_codepoint_boundary() {
        // Same pinned regression case as `truncate_walks_back_to_char_boundary_no_panic`
        // but exercising the in-place helper used directly by the
        // `message` field path. Previously `message` called
        // `String::truncate(MAX_FIELD_LEN)` raw and would panic here.
        let mut s = "a".repeat(MAX_FIELD_LEN - 2);
        s.push('💥');
        truncate_inplace(&mut s);
        assert!(s.ends_with("…[truncated]"));
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn truncate_passes_through_none() {
        assert_eq!(truncate(None), None);
    }

    #[test]
    fn truncate_passes_through_short_string() {
        let s = "a short error message".to_string();
        assert_eq!(truncate(Some(s.clone())), Some(s));
    }

    #[test]
    fn truncate_at_exactly_max_keeps_full_value() {
        // `>` not `>=` — a string of exactly MAX_FIELD_LEN should NOT
        // be truncated. Lock the boundary so a refactor to `>=` doesn't
        // start appending the marker to every max-length string.
        let s = "a".repeat(MAX_FIELD_LEN);
        let out = truncate(Some(s.clone())).unwrap();
        assert_eq!(out, s);
        assert!(!out.contains("…[truncated]"));
    }

    #[test]
    fn truncate_oversized_appends_marker() {
        let s = "a".repeat(MAX_FIELD_LEN + 100);
        let out = truncate(Some(s)).unwrap();
        // The marker is appended AFTER truncating to MAX_FIELD_LEN bytes,
        // so the final length is MAX_FIELD_LEN + marker.len().
        let marker = "…[truncated]";
        assert!(out.ends_with(marker));
        assert_eq!(out.len(), MAX_FIELD_LEN + marker.len());
    }

    #[test]
    fn truncate_walks_back_to_char_boundary_no_panic() {
        // Pre-fix, `String::truncate(4000)` panicked if byte 4000 fell
        // mid-codepoint. Construct exactly that case: 3998 ASCII bytes
        // followed by a 4-byte char so the boundary at byte 4000 is
        // mid-codepoint, and confirm we no longer crash.
        let mut s = "a".repeat(MAX_FIELD_LEN - 2);
        s.push('💥'); // 4 bytes → total = MAX + 2, byte 4000 is mid-char
        let out = truncate(Some(s)).unwrap();
        // The walked-back cut puts the marker at byte MAX_FIELD_LEN - 2.
        assert!(out.ends_with("…[truncated]"));
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_pure_ascii_unaffected_by_boundary_walk() {
        // For ASCII, every byte IS a char boundary — the walk-back is a
        // no-op. Cut should land exactly at MAX_FIELD_LEN.
        let s = "a".repeat(MAX_FIELD_LEN + 50);
        let out = truncate(Some(s)).unwrap();
        let marker = "…[truncated]";
        assert_eq!(out.len(), MAX_FIELD_LEN + marker.len());
    }
}
