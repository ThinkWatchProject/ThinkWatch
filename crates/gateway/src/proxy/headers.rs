//! Header helpers shared by all three AI surfaces. Trace/session id
//! extraction follows the same validation envelope as the access-log
//! middleware and the MCP transport layer — keep all three in sync if
//! you change one.

use axum::http::HeaderMap;

/// Resolve the trace id for an AI request. Prefers a caller-supplied
/// `x-trace-id` header (length-bounded ASCII, no control chars) so a
/// client that wants its AI request linked with a follow-on MCP
/// tools/call can pin both legs to one id. Falls back to a UUID.
pub(super) fn resolve_trace_id(headers: &HeaderMap) -> String {
    headers
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Resolve the multi-turn conversation id for an AI request from the
/// `x-session-id` header. Unlike `trace_id`, session_id is purely
/// optional — absent means the client isn't grouping turns, and we
/// store NULL so the ClickHouse `idx_session` bloom filter stays
/// selective for rows that do carry an id.
pub(super) fn resolve_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()))
}

/// Build a `HeaderValue` from a request id, falling back to a placeholder if
/// the id contains bytes outside the printable-ASCII range. Validation in
/// `RequestMetadata::extract` should prevent this, but we never want a
/// malformed id to crash the response path.
pub(super) fn request_id_header(request_id: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(request_id)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_trace_id_accepts_caller_supplied_value() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "abc-12345".parse().unwrap());
        assert_eq!(resolve_trace_id(&h), "abc-12345");
    }

    #[test]
    fn resolve_trace_id_trims_whitespace() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "  spaced  ".parse().unwrap());
        assert_eq!(resolve_trace_id(&h), "spaced");
    }

    #[test]
    fn resolve_trace_id_mints_uuid_when_missing() {
        let h = HeaderMap::new();
        let id = resolve_trace_id(&h);
        assert_eq!(id.len(), 36, "expected v4 UUID, got {id}");
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn resolve_trace_id_rejects_too_long_header() {
        let mut h = HeaderMap::new();
        let long = "x".repeat(129);
        h.insert("x-trace-id", long.parse().unwrap());
        let id = resolve_trace_id(&h);
        assert_ne!(id.len(), 129, "did not reject long header");
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn resolve_trace_id_rejects_empty_header() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "   ".parse().unwrap());
        let id = resolve_trace_id(&h);
        assert_eq!(id.len(), 36, "blank header should fall back to UUID");
    }

    /// 128-char boundary: exactly 128 should pass, 129 should reject.
    #[test]
    fn resolve_trace_id_boundary() {
        let mut h128 = HeaderMap::new();
        let s128 = "a".repeat(128);
        h128.insert("x-trace-id", s128.parse().unwrap());
        assert_eq!(resolve_trace_id(&h128).len(), 128);

        let mut h129 = HeaderMap::new();
        let s129 = "a".repeat(129);
        h129.insert("x-trace-id", s129.parse().unwrap());
        assert_eq!(resolve_trace_id(&h129).len(), 36, "129 must fall back");
    }
}
