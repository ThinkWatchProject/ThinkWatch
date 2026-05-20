//! `run_post_invoke` — orchestrates the four post-`invoke_upstream`
//! stages (`record_outcome` → `write_cache` → `record_usage` →
//! `emit_audit`) over a single [`Invoked`] state. The same function
//! is called from both the buffered foreground path and the
//! streaming detached task, so the audit emit happens exactly once
//! per request regardless of transport mode.
//!
//! Each stage delegates to a [`Surface`] trait method (see
//! [`Surface::record_outcome`] / [`Surface::write_cache`] /
//! [`Surface::record_usage`] / [`Surface::emit_audit`]); this
//! module owns the order, the "skip cache write when the stream
//! errored / was cancelled" gate, and the per-stage tracing spans.

use super::super::Surface;
use super::super::state::{Emitted, Invoked};

/// Run `record_outcome` then `write_cache` (when the view is a
/// success) then `emit_audit`, returning the terminal
/// [`Emitted`] state. For streaming `Emitted::response` is `None`
/// because the wire body was already flushed before the tail
/// resolved; for buffered it's `Some(response)`.
#[tracing::instrument(
    skip_all,
    fields(trace_id = %invoked.trace_id, candidate = %invoked.access_candidate),
)]
pub async fn run_post_invoke<S: Surface>(
    invoked: Invoked<S>,
    deps: &S::PostInvokeDeps,
) -> Emitted<S> {
    let invoked = record_outcome::<S>(invoked, deps).await;
    let invoked = write_cache::<S>(invoked, deps).await;
    let invoked = record_usage::<S>(invoked, deps).await;
    emit_audit::<S>(invoked, deps).await
}

/// Record success/failure against the surface's circuit breaker.
/// The trait impl owns the surface-specific classification rule
/// (caller-side JSON-RPC errors don't trip MCP's breaker; 5xx
/// upstream replies trip the AI gateway's). The stage itself only
/// owns the tracing span + the unconditional delegation.
#[tracing::instrument(skip_all, fields(trace_id = %invoked.trace_id))]
pub async fn record_outcome<S: Surface>(
    invoked: Invoked<S>,
    deps: &S::PostInvokeDeps,
) -> Invoked<S> {
    S::record_outcome(deps, &invoked).await;
    invoked
}

/// Write to cache when the captured view represents a successful
/// upstream interaction. Streaming non-Natural outcomes
/// (`UpstreamError`, `ClientCancelled`) never reach the surface
/// hook — a partial stream is by definition unsafe to cache.
/// Buffered always reaches the hook; the surface decides per-
/// response (e.g. don't cache an HTTP 5xx).
#[tracing::instrument(skip_all, fields(trace_id = %invoked.trace_id))]
pub async fn write_cache<S: Surface>(invoked: Invoked<S>, deps: &S::PostInvokeDeps) -> Invoked<S> {
    if invoked.view.is_success() {
        S::write_cache(deps, &invoked).await;
    }
    invoked
}

/// Debit limits / budget counters for the request's usage. Runs
/// after [`write_cache`] (so a cache write doesn't accidentally
/// re-fire on a counter rollback) and before [`emit_audit`] (so
/// the audit row sees post-debit counter values when the surface
/// chooses to embed them). MCP currently uses the default no-op
/// since `tools/call` doesn't have a token concept; the AI gateway
/// overrides the hook with `post_flight_account`.
#[tracing::instrument(skip_all, fields(trace_id = %invoked.trace_id))]
pub async fn record_usage<S: Surface>(invoked: Invoked<S>, deps: &S::PostInvokeDeps) -> Invoked<S> {
    S::record_usage(deps, &invoked).await;
    invoked
}

/// Emit the audit row (gateway_logs / mcp_logs). Single emit site
/// for the post-invoke path. The terminal [`Emitted`] carries the
/// wire response for the buffered case so the surface handler can
/// hand it to axum; for streaming it carries `None` because the
/// wire body was already flushed before this stage ran.
#[tracing::instrument(skip_all, fields(trace_id = %invoked.trace_id))]
pub async fn emit_audit<S: Surface>(invoked: Invoked<S>, deps: &S::PostInvokeDeps) -> Emitted<S> {
    S::emit_audit(deps, &invoked).await;
    let response = match invoked.view {
        super::super::state::CapturedView::Buffered(response) => Some(response),
        super::super::state::CapturedView::Streaming { .. } => None,
    };
    Emitted { response }
}

#[cfg(test)]
mod tests {
    use super::super::super::StreamOutcome;
    use super::super::super::test_surface::{
        TestDeps, TestResponse, TestSurface, make_buffered_invoked, make_streaming_invoked,
    };
    use super::*;
    use uuid::Uuid;

    /// Buffered path runs all four hooks in order and returns the
    /// wire response inside `Emitted.response` so the surface
    /// handler can hand it to axum.
    #[tokio::test]
    async fn buffered_runs_all_hooks_and_propagates_response() {
        let user_id = Uuid::new_v4();
        let invoked = make_buffered_invoked(user_id, TestResponse::Ok);
        let deps = TestDeps::default();

        let emitted = run_post_invoke::<TestSurface>(invoked, &deps).await;

        assert_eq!(deps.record_outcome(), 1);
        assert_eq!(deps.write_cache(), 1);
        assert_eq!(deps.record_usage(), 1);
        assert_eq!(deps.emit_audit(), 1);
        assert_eq!(
            deps.order(),
            vec![
                "record_outcome",
                "write_cache",
                "record_usage",
                "emit_audit"
            ],
            "post-invoke stage order is \
             record_outcome → write_cache → record_usage → emit_audit"
        );
        assert_eq!(deps.write_cache_kind(), Some("buffered"));
        assert_eq!(
            emitted.response,
            Some(TestResponse::Ok),
            "buffered path returns the wire response so the surface can flush it"
        );
    }

    /// Streaming/Natural: cache write IS called, usage debit AND
    /// audit emit run, but `Emitted.response` is `None` because the
    /// SSE body was already on the wire before the tail resolved.
    #[tokio::test]
    async fn streaming_natural_writes_cache_with_no_terminal_response() {
        let user_id = Uuid::new_v4();
        let invoked = make_streaming_invoked(user_id, StreamOutcome::Natural);
        let deps = TestDeps::default();

        let emitted = run_post_invoke::<TestSurface>(invoked, &deps).await;

        assert_eq!(deps.write_cache(), 1);
        assert_eq!(deps.write_cache_kind(), Some("streaming"));
        assert_eq!(deps.record_usage(), 1);
        assert_eq!(deps.emit_audit(), 1);
        assert!(
            emitted.response.is_none(),
            "streaming response is already on the wire before the tail; \
             Emitted.response must be None so the surface handler doesn't \
             accidentally double-flush"
        );
    }

    /// ClientCancelled: cache write is SKIPPED (partial response
    /// would poison subsequent callers), but record_outcome,
    /// record_usage, and emit_audit still run so the request is
    /// fully accounted for (the tokens were generated even if the
    /// client left).
    #[tokio::test]
    async fn streaming_client_cancelled_skips_cache_but_still_audits() {
        let user_id = Uuid::new_v4();
        let invoked = make_streaming_invoked(user_id, StreamOutcome::ClientCancelled);
        let deps = TestDeps::default();

        let _ = run_post_invoke::<TestSurface>(invoked, &deps).await;

        assert_eq!(deps.record_outcome(), 1, "breaker accounting still runs");
        assert_eq!(
            deps.write_cache(),
            0,
            "client cancellation = partial response = unsafe to cache"
        );
        assert_eq!(deps.record_usage(), 1, "usage debit still runs");
        assert_eq!(deps.emit_audit(), 1, "audit row always emits");
        assert_eq!(
            deps.order(),
            vec!["record_outcome", "record_usage", "emit_audit"],
            "cache stage skipped; the remaining three still fire in order"
        );
    }

    /// UpstreamError: same skip rule as ClientCancelled — a stream
    /// that errored has no canonical body to cache. record_usage
    /// still fires so partial-token debits land.
    #[tokio::test]
    async fn streaming_upstream_error_skips_cache() {
        let user_id = Uuid::new_v4();
        let invoked = make_streaming_invoked(
            user_id,
            StreamOutcome::UpstreamError {
                error_type: "NetworkError".into(),
                message: "connection reset".into(),
                status_code: 502,
            },
        );
        let deps = TestDeps::default();

        let _ = run_post_invoke::<TestSurface>(invoked, &deps).await;

        assert_eq!(deps.record_outcome(), 1);
        assert_eq!(deps.write_cache(), 0);
        assert_eq!(deps.record_usage(), 1);
        assert_eq!(deps.emit_audit(), 1);
    }
}
