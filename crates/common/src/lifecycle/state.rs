//! Per-stage state structs. Each stage consumes one struct and
//! returns the next, encoding the lifecycle progression in the
//! type system: you literally cannot call `check_access` against a
//! [`Raw`] state because the signature takes `LimitsChecked`.
//!
//! The transitions move ownership, not clone. Each subsequent
//! struct destructures the previous one and adds the new field(s).
//! Stage code looks like:
//!
//! ```ignore
//! let Raw { identity, trace_id, started_at, client_ip } = state;
//! let limit_check = run_limit_check(&identity, &redis, &rules).await?;
//! Ok(LimitsChecked { identity, trace_id, started_at, client_ip, limit_check })
//! ```
//!
//! Verbose by design — the destructure makes every field carried
//! across the transition visible at the call site. Adding a field
//! to `Raw` that should also live in `LimitsChecked` is a localised
//! edit (one struct + one transition fn).

use std::pin::Pin;
use std::time::Instant;

use super::Surface;
use super::streaming::StreamOutcome;

/// Initial state. Identity has been resolved by HTTP middleware;
/// nothing else has happened yet.
///
/// Note: the typed request body (`ChatCompletionRequest`,
/// `JsonRpcRequest`, …) is NOT carried through the lifecycle
/// state — surface handlers keep it as a local variable instead.
/// Stages haven't needed to inspect bodies in any of the four
/// surfaces shipped so far (the candidate string for `check_access`
/// is passed in as an explicit `&str`); when one does, add a
/// dedicated `body: S::RequestBody` field at that point rather
/// than carrying ceremonial plumbing.
pub struct Raw<S: Surface> {
    /// Per-surface identity (carries `SurfaceConstraints`, etc.)
    pub identity: S::Identity,
    /// Trace id — minted by HTTP middleware or pinned via
    /// `x-trace-id` header. Stamped on every audit row this
    /// pipeline produces.
    pub trace_id: String,
    /// Wall-clock pipeline start. Used to compute `duration_ms`
    /// for the audit row.
    pub started_at: Instant,
    /// Resolved client IP. May be `None` when extraction failed.
    pub client_ip: Option<String>,
}

impl<S: Surface> Raw<S> {
    pub fn new(identity: S::Identity, trace_id: String, client_ip: Option<String>) -> Self {
        Self {
            identity,
            trace_id,
            started_at: Instant::now(),
            client_ip,
        }
    }
}

/// Output of `check_limits`. Carries everything from [`Raw`] plus
/// the materialised limit-check result so downstream stages (in
/// particular `emit_audit`) can record what budgets / counters
/// this request charged.
pub struct LimitsChecked<S: Surface> {
    pub identity: S::Identity,
    pub trace_id: String,
    pub started_at: Instant,
    pub client_ip: Option<String>,
    /// Outcome of the rate-limit check. For `check_limits` to
    /// emit `LimitsChecked` at all, the check must have *passed*;
    /// this field records the per-window currents the audit row
    /// surfaces in its `limits` block.
    pub limit_check: LimitCheckRecord,
}

/// Successful-check trace data. Mirrors the shape
/// [`crate::limits::sliding::CheckOutcome`] returns, minus the
/// "allowed: bool" — if we're carrying this in a `LimitsChecked`
/// we already know it was allowed.
#[derive(Debug, Clone)]
pub struct LimitCheckRecord {
    /// Current count after the increment, per resolved rule.
    /// Position matches the input rule order so the audit row
    /// can pair them back up.
    pub currents: Vec<i64>,
}

/// Output of `check_access`. Same field set as
/// [`LimitsChecked`] — the access stage doesn't add new data, it
/// just narrows the type so subsequent stages can't be reached
/// without it. Carries the `candidate` string the access decision
/// was made against (tool name for MCP, model name for the AI
/// gateway) so downstream audit can record what was authorized.
pub struct Authorized<S: Surface> {
    pub identity: S::Identity,
    pub trace_id: String,
    pub started_at: Instant,
    pub client_ip: Option<String>,
    pub limit_check: LimitCheckRecord,
    /// Surface-specific candidate the access check ran against —
    /// a tool name (`stream__test_tool`) for MCP, a model id
    /// (`gpt-4o-mini`) for the AI gateway. Audit emit will surface
    /// this on the row's `detail.access.subject` field.
    pub access_candidate: String,
}

/// What `invoke_upstream` produced. Buffered hands the full
/// response back; streaming hands back the wire body (already
/// wrapped as SSE) plus a future that resolves to the materialised
/// [`Invoked`] when the stream ends.
///
/// The surface handler matches on this:
/// - [`Buffered`] → run [`super::stages::run_post_invoke`] in the
///   foreground; return `Emitted.response` to the caller.
/// - [`Streaming`] → return `response` to axum synchronously, and
///   `tokio::spawn` a task that `await`s `tail` and then calls
///   `run_post_invoke` on the resulting [`Invoked`]. The detached
///   task is the single audit-emit site for the streaming path.
///
/// [`Buffered`]: Invocation::Buffered
/// [`Streaming`]: Invocation::Streaming
pub enum Invocation<S: Surface> {
    Buffered(Invoked<S>),
    Streaming {
        /// Already-wrapped streaming wire body (an SSE payload,
        /// not a [`Surface::Response`]) the surface returns to
        /// axum before the post-call tail finishes.
        response: S::StreamResponse,
        /// Resolves exactly once, when the stream terminates
        /// (natural, error, or client drop). The output is the
        /// fully-populated state ready for `run_post_invoke`.
        tail: Pin<Box<dyn std::future::Future<Output = Invoked<S>> + Send>>,
    },
}

/// Either the buffered response that came back from the upstream
/// or the streaming-mode capture. The post-invoke stages
/// (`record_outcome` / `write_cache` / `emit_audit`) match on this
/// to derive their inputs without caring which transport mode
/// produced them.
pub enum CapturedView<S: Surface> {
    /// Upstream produced a complete response in one shot.
    Buffered(S::Response),
    /// Upstream streamed. `captured` is the surface-defined shape
    /// the pump accumulated (`Vec<ChatCompletionChunk>` for the AI
    /// gateway, `Vec<serde_json::Value>` for MCP). `outcome`
    /// carries why the stream ended.
    Streaming {
        outcome: StreamOutcome,
        captured: S::StreamCaptured,
    },
}

impl<S: Surface> CapturedView<S> {
    /// True for `Buffered` and for `Streaming` that ended naturally.
    /// The post-invoke stages use this to gate cache-write decisions
    /// (don't cache partial / errored streams).
    pub fn is_success(&self) -> bool {
        match self {
            CapturedView::Buffered(_) => true,
            CapturedView::Streaming { outcome, .. } => outcome.is_natural(),
        }
    }
}

/// Output of `invoke_upstream` — the full carry-over state plus
/// the captured upstream view. Consumed by the post-invoke stages
/// in turn: [`super::stages::record_outcome`] →
/// [`super::stages::write_cache`] → [`super::stages::emit_audit`].
///
/// The streaming branch builds this *inside* the detached tail
/// future, then hands the foreground response back to axum before
/// any post-invoke work runs.
pub struct Invoked<S: Surface> {
    pub identity: S::Identity,
    pub trace_id: String,
    pub started_at: Instant,
    pub client_ip: Option<String>,
    pub limit_check: LimitCheckRecord,
    pub access_candidate: String,
    /// Buffered response OR streaming capture + outcome.
    pub view: CapturedView<S>,
}

/// Terminal state. `emit_audit` returns this; the surface handler
/// unwraps `response` to send on the wire (buffered) or discards
/// it (streaming, where the response is already on the wire).
pub struct Emitted<S: Surface> {
    /// The surface's wire-format response. For streaming, this is
    /// `None` because the SSE body was already flushed; for
    /// buffered it's `Some(response)`.
    pub response: Option<S::Response>,
}
