//! `check_breaker` — short-circuit a `tools/call` when the
//! per-server MCP circuit breaker is Open. Same shape as
//! [`super::check_cache`] — `Authorized` passes through on the
//! closed/half-open path, short-circuits with an `INTERNAL_ERROR`
//! JSON-RPC response on Open.
//!
//! The breaker is keyed by server UUID (NOT name) — see
//! [`crate::circuit_breaker::McpCircuitBreakers`] for why. The
//! display name is still threaded through so the audit row and
//! short-circuit error message stay human-readable; this stage
//! doesn't decide the key, the surface code hands it in.
//! Pre-migration inline code emitted a `warn!` log but no audit
//! row; this stage emits a `"breaker_open"` row so the deny
//! shows up on the trace UI.

use uuid::Uuid;

use think_watch_common::audit::{AuditActor, AuditLogger, McpActor};
use think_watch_common::lifecycle::state::Authorized;

use crate::circuit_breaker::McpCircuitBreakers;
use crate::lifecycle::McpSurface;
use crate::proxy::{INTERNAL_ERROR, JsonRpcResponse, err_response};

// clippy::result_large_err — measured, not waved away: the `Ok` variant
// `Authorized<McpSurface>` is 296 bytes and the `Err` variant
// `JsonRpcResponse` is 152, so the `Result` is sized by `Ok` at 296
// either way. `Result<_, Box<JsonRpcResponse>>` also measures 296 —
// boxing saves exactly zero bytes here and adds an allocation on the
// short-circuit path. The `Err` is a short-circuit response, not an
// error; the caller mutates its `id` and hands it straight to
// `HandleOutcome::Buffered` by value.
#[allow(clippy::result_large_err)]
#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, server = %server_name),
)]
pub async fn check_breaker(
    state: Authorized<McpSurface>,
    breakers: &McpCircuitBreakers,
    server_id: Uuid,
    server_name: &str,
    audit: &AuditLogger,
) -> Result<Authorized<McpSurface>, JsonRpcResponse> {
    if breakers.check(server_id, server_name).is_err() {
        metrics::counter!("lifecycle_breaker_short_circuit_total").increment(1);
        tracing::warn!(
            trace_id = %state.trace_id,
            server = %server_name,
            "tools/call short-circuited: MCP circuit breaker open"
        );
        let entry = McpActor {
            user_id: state.identity.user_id,
            user_email: &state.identity.user_email,
            ip: state.identity.ip_address.as_deref(),
        }
        .audit("tools.call.breaker_open")
        .trace_id(state.trace_id.clone())
        .detail(serde_json::json!({
            "server_name": server_name,
            "tool": state.access_candidate.clone(),
        }));
        audit.log(entry);
        return Err(err_response(
            None,
            INTERNAL_ERROR,
            format!("Upstream MCP server '{server_name}' is temporarily unavailable"),
        ));
    }
    Ok(state)
}
