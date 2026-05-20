//! `check_breaker` — short-circuit a `tools/call` when the
//! per-server MCP circuit breaker is Open. Same shape as
//! [`super::check_cache`] — `Authorized` passes through on the
//! closed/half-open path, short-circuits with an `INTERNAL_ERROR`
//! JSON-RPC response on Open.
//!
//! The breaker is keyed by server name (what the dashboard's
//! upstream-health panel reads from the shared `cb_registry`);
//! this stage doesn't decide the key, the surface code hands it
//! in. Pre-migration inline code emitted a `warn!` log but no
//! audit row; this stage emits a `"breaker_open"` row so the
//! deny shows up on the trace UI.

use think_watch_common::audit::{AuditActor, AuditLogger, McpActor};
use think_watch_common::lifecycle::state::Authorized;

use crate::circuit_breaker::McpCircuitBreakers;
use crate::lifecycle::McpSurface;
use crate::proxy::{INTERNAL_ERROR, JsonRpcResponse, err_response};

#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, server = %server_name),
)]
pub async fn check_breaker(
    state: Authorized<McpSurface>,
    breakers: &McpCircuitBreakers,
    server_name: &str,
    audit: &AuditLogger,
) -> Result<Authorized<McpSurface>, JsonRpcResponse> {
    if breakers.check(server_name).await.is_err() {
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
