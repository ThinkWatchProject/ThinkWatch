//! `check_cache` — MCP response cache lookup as a lifecycle
//! stage. On hit, emits a `"cache_hit"` audit row and short-
//! circuits with the cached JSON-RPC response. On miss (or
//! caching disabled) passes the [`Authorized`] state through
//! unchanged.
//!
//! Cache scope (Global vs PerCaller) and TTL come from the
//! server's persisted config — this stage doesn't decide either,
//! the surface code resolves them and hands them in. Same shape
//! as the pre-migration inline block, just lifted into a named
//! stage so the audit emit happens uniformly with the other
//! short-circuit stages.

use uuid::Uuid;

use think_watch_common::audit::{AuditActor, McpActor};
use think_watch_common::lifecycle::state::Authorized;

use crate::cache::{CallerScope, McpResponseCache};
use crate::lifecycle::McpSurface;
use crate::proxy::{JsonRpcRequest, JsonRpcResponse};

#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, server_id = %server_id, ttl_secs = cache_ttl_secs),
)]
pub async fn check_cache(
    state: Authorized<McpSurface>,
    cache: &McpResponseCache,
    server_id: Uuid,
    cache_scope: Option<CallerScope<'_>>,
    cache_ttl_secs: u64,
    upstream_request: &JsonRpcRequest,
    audit: &think_watch_common::audit::AuditLogger,
) -> Result<Authorized<McpSurface>, JsonRpcResponse> {
    // Caching disabled (TTL == 0) ⇒ pass-through. Skip the Redis
    // round-trip entirely.
    if cache_ttl_secs == 0 {
        return Ok(state);
    }

    match cache.get(&server_id, cache_scope, upstream_request).await {
        Some(cached) => {
            metrics::counter!("mcp_cache_hits_total").increment(1);
            tracing::debug!(
                trace_id = %state.trace_id,
                server_id = %server_id,
                "MCP cache hit"
            );
            // Audit emit so cache hits show up alongside misses
            // in the trace UI. Pre-migration inline code emitted
            // nothing for cache hits (the audit row was only
            // produced for misses-that-completed); this is the
            // small observability win that comes free with the
            // shared short-circuit pattern.
            let entry = McpActor {
                user_id: state.identity.user_id,
                user_email: &state.identity.user_email,
                ip: state.identity.ip_address.as_deref(),
            }
            .audit("tools.call.cache_hit")
            .trace_id(state.trace_id.clone())
            .detail(serde_json::json!({
                "server_id": server_id.to_string(),
                "tool": state.access_candidate.clone(),
            }));
            audit.log(entry);
            Err(cached)
        }
        None => {
            metrics::counter!("mcp_cache_misses_total").increment(1);
            Ok(state)
        }
    }
}
