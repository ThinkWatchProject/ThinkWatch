use fred::clients::Client;
use fred::interfaces::KeysInterface;
use uuid::Uuid;
use xxhash_rust::xxh3::xxh3_128;

use crate::proxy::{JsonRpcRequest, JsonRpcResponse};

const KEY_PREFIX: &str = "mcp_cache:";

/// What dimensions of the caller's identity the cache key includes.
///
/// Passed to [`McpResponseCache::get`] / [`McpResponseCache::set`] per
/// request. The server-level [`crate::registry::ServerCacheScope`]
/// decides whether the caller dimension applies at all:
///
/// - `Global` server (public / fixed-header service-to-service) → pass
///   `None`. All callers share one cache lane.
/// - `PerCaller` server (OAuth / static-token / `{{user_id}}` headers) →
///   pass `Some(CallerScope { user_id, account_label })`. Each caller
///   gets their own lane, with multi-account users (e.g. personal +
///   work GitHub) further split by `account_label`.
#[derive(Debug, Clone, Copy)]
pub struct CallerScope<'a> {
    pub user_id: &'a Uuid,
    /// `Some(label)` when the caller is routing to a specific named
    /// credential (multi-account OAuth / static-token); `None` for the
    /// user's default credential. The label string is whatever the API
    /// key passed in `mcp_account_overrides[<server_id>]`.
    pub account_label: Option<&'a str>,
}

/// Redis-based exact-match cache for MCP tool call responses.
///
/// Cache keys are semantic: `server_id + caller (optional) + method +
/// params`, where the caller dimension is included only when the
/// upstream server forwards caller identity (see [`CallerScope`] +
/// [`crate::registry::ServerCacheScope`]).
///
/// - **Global lane** (no caller dimension): one entry shared across
///   every user. Safe for public MCPs and for services authed by a
///   fixed shared header (e.g. `X-API-Key: <secret>`).
/// - **Per-caller lane** (caller dimension mixed in): one entry per
///   `(user_id, account_label?)`. Required for OAuth, static token,
///   and `{{user_id}}`-templated headers — the upstream sees the
///   caller's own credential and may return different data per user.
#[derive(Clone)]
pub struct McpResponseCache {
    redis: Client,
}

impl McpResponseCache {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Build a deterministic cache key.
    ///
    /// The caller dimension is mixed in only when `caller` is `Some`.
    /// Within a per-caller lane, two requests from the same user but
    /// with different `account_label`s land in distinct entries — that
    /// matters for users who have multiple credentials per server
    /// (e.g. personal + work GitHub).
    pub fn cache_key(
        server_id: &Uuid,
        caller: Option<CallerScope<'_>>,
        request: &JsonRpcRequest,
    ) -> String {
        let params_json = request
            .params
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap_or_default())
            .unwrap_or_default();

        let mut input = Vec::with_capacity(256);
        input.extend_from_slice(server_id.as_bytes());
        if let Some(c) = caller {
            input.push(b':');
            input.extend_from_slice(c.user_id.as_bytes());
            // Mix `account_label` into the hash so personal vs work
            // credentials can't collide. We always emit a separator
            // even for `None` so a label "" can't collide with absent.
            input.push(b'|');
            if let Some(label) = c.account_label {
                input.extend_from_slice(label.as_bytes());
            }
        }
        input.push(b':');
        input.extend_from_slice(request.method.as_bytes());
        input.push(b':');
        input.extend_from_slice(params_json.as_bytes());

        let hash = xxh3_128(&input);
        format!("{KEY_PREFIX}{hash:032x}")
    }

    /// Look up a cached response.
    pub async fn get(
        &self,
        server_id: &Uuid,
        caller: Option<CallerScope<'_>>,
        request: &JsonRpcRequest,
    ) -> Option<JsonRpcResponse> {
        let scope_label = scope_metric_label(caller);
        let key = Self::cache_key(server_id, caller, request);
        let cached: Option<String> = self.redis.get(&key).await.ok().flatten();
        let hit = cached.is_some();
        let parsed = cached.and_then(|json| {
            serde_json::from_str::<JsonRpcResponse>(&json)
                .map_err(|e| {
                    tracing::warn!("Failed to deserialize cached MCP response: {e}");
                    e
                })
                .ok()
        });
        // Three counters per lookup, each labelled by scope so the
        // dashboard can break hit-rate down by global vs per-caller:
        //   * mcp_cache_total        — denominator
        //   * mcp_cache_hit_total    — numerator (only when parse OK)
        //   * mcp_cache_parse_miss_total — JSON we stored that we can't
        //                                  read back; would otherwise
        //                                  look like a miss
        //   * mcp_cache_miss_total   — true miss
        metrics::counter!("mcp_cache_total", "scope" => scope_label).increment(1);
        match (hit, parsed.is_some()) {
            (true, true) => {
                metrics::counter!("mcp_cache_hit_total", "scope" => scope_label).increment(1)
            }
            (true, false) => {
                metrics::counter!("mcp_cache_parse_miss_total", "scope" => scope_label).increment(1)
            }
            (false, _) => {
                metrics::counter!("mcp_cache_miss_total", "scope" => scope_label).increment(1)
            }
        }
        parsed
    }

    /// Store a response in the cache with the given TTL (in seconds).
    pub async fn set(
        &self,
        server_id: &Uuid,
        caller: Option<CallerScope<'_>>,
        request: &JsonRpcRequest,
        response: &JsonRpcResponse,
        ttl_secs: u64,
    ) {
        // Don't cache error responses.
        if response.error.is_some() {
            return;
        }

        let scope_label = scope_metric_label(caller);
        let key = Self::cache_key(server_id, caller, request);
        let json = match serde_json::to_string(response) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to serialize MCP response for cache: {e}");
                return;
            }
        };

        let expiration = fred::types::Expiration::EX(ttl_secs as i64);
        let result: Result<(), _> = self
            .redis
            .set(&key, json.as_str(), Some(expiration), None, false)
            .await;

        match result {
            Ok(()) => {
                metrics::counter!("mcp_cache_store_total", "scope" => scope_label).increment(1)
            }
            Err(e) => {
                tracing::warn!("Failed to cache MCP response: {e}");
                metrics::counter!("mcp_cache_store_error_total", "scope" => scope_label)
                    .increment(1);
            }
        }
    }
}

/// Static label for the metrics `scope` dimension.
fn scope_metric_label(caller: Option<CallerScope<'_>>) -> &'static str {
    match caller {
        None => "global",
        Some(CallerScope {
            account_label: None,
            ..
        }) => "per_user",
        Some(CallerScope {
            account_label: Some(_),
            ..
        }) => "per_credential",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str, name: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Some(serde_json::json!(1)),
            method: method.to_owned(),
            params: Some(serde_json::json!({ "name": name, "arguments": {} })),
        }
    }

    fn caller<'a>(user_id: &'a Uuid, account_label: Option<&'a str>) -> CallerScope<'a> {
        CallerScope {
            user_id,
            account_label,
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let sid = Uuid::new_v4();
        let req = make_request("tools/call", "mysql__query");
        let k1 = McpResponseCache::cache_key(&sid, None, &req);
        let k2 = McpResponseCache::cache_key(&sid, None, &req);
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_servers_produce_different_keys() {
        let req = make_request("tools/call", "mysql__query");
        let k1 = McpResponseCache::cache_key(&Uuid::new_v4(), None, &req);
        let k2 = McpResponseCache::cache_key(&Uuid::new_v4(), None, &req);
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_params_produce_different_keys() {
        let sid = Uuid::new_v4();
        let k1 = McpResponseCache::cache_key(&sid, None, &make_request("tools/call", "tool_a"));
        let k2 = McpResponseCache::cache_key(&sid, None, &make_request("tools/call", "tool_b"));
        assert_ne!(k1, k2);
    }

    #[test]
    fn caller_scoped_key_differs_from_global() {
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let global = McpResponseCache::cache_key(&sid, None, &req);
        let scoped = McpResponseCache::cache_key(&sid, Some(caller(&uid, None)), &req);
        assert_ne!(global, scoped);
    }

    #[test]
    fn different_users_produce_different_keys() {
        let sid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let k1 = McpResponseCache::cache_key(&sid, Some(caller(&u1, None)), &req);
        let k2 = McpResponseCache::cache_key(&sid, Some(caller(&u2, None)), &req);
        assert_ne!(k1, k2);
    }

    #[test]
    fn same_user_different_account_label_produces_different_keys() {
        // Multi-account case: same user, two GitHub credentials
        // (personal vs work). Keys must differ so cross-account
        // responses can't collide.
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "list_repos");
        let personal =
            McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("personal"))), &req);
        let work = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("work"))), &req);
        assert_ne!(personal, work);
    }

    #[test]
    fn account_label_empty_string_collapses_to_none() {
        // `Some("")` is a degenerate label — practically equivalent to
        // "default credential" / `None`. We don't pretend they're
        // distinct because no real API key config would emit `""`,
        // and pretending requires extra sentinel bytes for no benefit.
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let none_label = McpResponseCache::cache_key(&sid, Some(caller(&uid, None)), &req);
        let empty_label = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some(""))), &req);
        assert_eq!(none_label, empty_label);
    }

    #[test]
    fn key_has_expected_prefix() {
        let key =
            McpResponseCache::cache_key(&Uuid::new_v4(), None, &make_request("tools/call", "x"));
        assert!(key.starts_with("mcp_cache:"), "got {key}");
    }

    #[test]
    fn scope_metric_labels() {
        let uid = Uuid::new_v4();
        assert_eq!(scope_metric_label(None), "global");
        assert_eq!(scope_metric_label(Some(caller(&uid, None))), "per_user");
        assert_eq!(
            scope_metric_label(Some(caller(&uid, Some("work")))),
            "per_credential"
        );
    }
}
