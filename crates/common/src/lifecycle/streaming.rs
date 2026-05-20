//! Surface-agnostic streaming primitives. Phase 2 of the lifecycle
//! refactor (see [`STREAMING.md`](./STREAMING.md)) lifts the two
//! per-surface `StreamOutcome` enums into this single shared type
//! so the audit / cache / breaker tail in [`super::stages`] doesn't
//! need to know which gateway it's running for.
//!
//! The AI gateway's `gateway::streaming::StreamOutcome` and the MCP
//! gateway's `mcp_gateway::proxy::streaming::StreamOutcome` are
//! **deleted** in the same commits that migrate each surface; this
//! module is the single source of truth.
//!
//! User-attributable rate-limit / access-denied outcomes are NOT
//! streaming concerns — they short-circuit before `invoke_upstream`
//! ever runs, per DESIGN.md §3.

use serde_json::Value;

/// How an upstream stream terminated. Drives audit-time status
/// classification and breaker accounting in the detached post-call
/// task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// Upstream emitted its terminator (`[DONE]` for OpenAI SSE,
    /// graceful EOF for MCP) and the consumer received every chunk.
    Natural,
    /// Upstream produced an error mid-stream, OR the transport
    /// errored before any chunks could be forwarded. `status_code`
    /// carries the canonical HTTP status the wire would have shown
    /// had the error occurred before SSE headers flushed — so a
    /// throttled upstream stays 429 on the audit row, not 502.
    UpstreamError {
        /// Short tag (e.g. `"NetworkError"`, `"ProviderTimeout"`,
        /// `"transport"`) that works as a Prometheus label value.
        error_type: String,
        /// Operator-facing description.
        message: String,
        /// HTTP status to log against the audit row.
        status_code: i64,
    },
    /// Consumer dropped the stream future before completion. Counts
    /// as a success against the upstream — the client left, the
    /// upstream did nothing wrong — but the partial response is not
    /// cached.
    ClientCancelled,
}

impl StreamOutcome {
    /// True when the stream ran end-to-end without error.
    pub fn is_natural(&self) -> bool {
        matches!(self, StreamOutcome::Natural)
    }

    /// `(logged_status, optional_detail_blob)` for the audit row.
    ///
    /// - `Natural` → 200, no extra detail.
    /// - `UpstreamError` → carried `status_code`, detail carries
    ///   the type + message so dashboards can split provider drops
    ///   from rate-limit errors.
    /// - `ClientCancelled` → 499 (nginx convention for "client
    ///   closed connection"; the HTTP catalogue has no standard
    ///   slot for it but 499 is what every observability dashboard
    ///   already filters on).
    pub fn logged_status_and_detail(&self) -> (i64, Option<Value>) {
        match self {
            StreamOutcome::Natural => (200, None),
            StreamOutcome::UpstreamError {
                error_type,
                message,
                status_code,
            } => (
                *status_code,
                Some(serde_json::json!({
                    "error_type": error_type,
                    "error_message": message,
                    "stream_outcome": "upstream_error",
                })),
            ),
            StreamOutcome::ClientCancelled => (
                499,
                Some(serde_json::json!({
                    "stream_outcome": "client_cancelled",
                })),
            ),
        }
    }

    /// Short metric label (`"natural"`, `"upstream_error"`,
    /// `"cancelled"`) for Prometheus. Centralised here so the two
    /// surfaces report on the same label set.
    pub fn metric_label(&self) -> &'static str {
        match self {
            StreamOutcome::Natural => "natural",
            StreamOutcome::UpstreamError { .. } => "upstream_error",
            StreamOutcome::ClientCancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_logs_200() {
        let (status, detail) = StreamOutcome::Natural.logged_status_and_detail();
        assert_eq!(status, 200);
        assert!(detail.is_none());
    }

    #[test]
    fn upstream_error_preserves_status_code() {
        let outcome = StreamOutcome::UpstreamError {
            error_type: "ProviderTimeout".into(),
            message: "504 from upstream".into(),
            status_code: 504,
        };
        let (status, detail) = outcome.logged_status_and_detail();
        assert_eq!(status, 504, "audit row must preserve upstream HTTP status");
        let detail = detail.expect("error detail required");
        assert_eq!(detail["error_type"], "ProviderTimeout");
        assert_eq!(detail["stream_outcome"], "upstream_error");
    }

    #[test]
    fn client_cancelled_logs_499() {
        let (status, detail) = StreamOutcome::ClientCancelled.logged_status_and_detail();
        assert_eq!(status, 499);
        assert_eq!(detail.unwrap()["stream_outcome"], "client_cancelled");
    }

    #[test]
    fn metric_labels_disjoint() {
        assert_eq!(StreamOutcome::Natural.metric_label(), "natural");
        assert_eq!(
            StreamOutcome::UpstreamError {
                error_type: "x".into(),
                message: "y".into(),
                status_code: 500,
            }
            .metric_label(),
            "upstream_error",
        );
        assert_eq!(StreamOutcome::ClientCancelled.metric_label(), "cancelled",);
    }

    #[test]
    fn is_natural_only_for_natural() {
        assert!(StreamOutcome::Natural.is_natural());
        assert!(!StreamOutcome::ClientCancelled.is_natural());
        assert!(
            !StreamOutcome::UpstreamError {
                error_type: "x".into(),
                message: "y".into(),
                status_code: 500,
            }
            .is_natural()
        );
    }
}
