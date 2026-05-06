use fred::clients::Client;
use fred::interfaces::KeysInterface;
use fred::types::scan::ScanType;
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
/// Cache keys are structured for **prefix-based invalidation**:
///
/// ```text
/// mcp_cache:<server_hex>:<user_part>:<label_part>:<request_hash>
/// ```
///
/// where `<user_part>` is the user UUID (simple hex) or `_` for the
/// shared global lane, and `<label_part>` is hex-encoded
/// `account_label` or `_` when the caller didn't route to a named
/// credential. Each segment is hex-only so `:` can never appear inside
/// a segment, making `SCAN MATCH mcp_cache:<server>:<user>:*:*` safe.
///
/// Lanes:
/// - **Global lane** (`_:_`): one entry shared across every user. Safe
///   for public MCPs and fixed-header service-to-service auth.
/// - **Per-caller lane** (`<user>:<label>`): one entry per
///   `(user_id, account_label?)`. Required for OAuth, static token,
///   and `{{user_id}}`-templated headers — the upstream sees the
///   caller's own credential and may return different data per user.
///
/// [`McpResponseCache::invalidate_user_lane`] uses the prefix-able
/// shape to wipe a single user's cached responses for one server when
/// their credential rotates / is revoked. Without that hook, post-
/// rotation upstream calls would tunnel through pre-rotation cache
/// entries until TTL expired.
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
    /// See the type-level doc for the key shape and rationale.
    pub fn cache_key(
        server_id: &Uuid,
        caller: Option<CallerScope<'_>>,
        request: &JsonRpcRequest,
    ) -> String {
        let server_part = server_id.simple().to_string();
        let (user_part, label_part) = caller_key_parts(caller);

        // Hash method + params separately so the request component is
        // a fixed-width hex segment regardless of payload size.
        let req_hash = {
            let params_json = request
                .params
                .as_ref()
                .map(|p| serde_json::to_string(p).unwrap_or_default())
                .unwrap_or_default();
            let mut input = Vec::with_capacity(64 + params_json.len());
            input.extend_from_slice(request.method.as_bytes());
            input.push(b':');
            input.extend_from_slice(params_json.as_bytes());
            format!("{:032x}", xxh3_128(&input))
        };

        format!("{KEY_PREFIX}{server_part}:{user_part}:{label_part}:{req_hash}")
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

    /// Wipe every cached response for `(server_id, user_id)` across all
    /// `account_label` lanes (including the no-label `_` lane).
    ///
    /// Called by the credential lifecycle: any time a user's OAuth
    /// token is refreshed, replaced, or revoked, OR when an admin
    /// rotates the upstream client credentials, the cached responses
    /// became stale (they reflect the old upstream identity). Without
    /// this hook, the response cache acts as a tunnel from the old
    /// epoch into the new one until TTL elapses — a real cross-epoch
    /// data-leak window.
    ///
    /// Implementation: SCAN with `mcp_cache:<server>:<user>:*:*`,
    /// DEL the matching keys in batches. `SCAN` is non-blocking on
    /// the Redis side and safe to run concurrently with normal traffic.
    /// Conservative on coverage — also clears `<user>:_` (the "no
    /// label" lane) so a refresh of a user's *default* credential
    /// invalidates entries cached when no override was passed at
    /// request time.
    pub async fn invalidate_user_lane(&self, server_id: &Uuid, user_id: &Uuid) {
        let pattern = format!("{KEY_PREFIX}{}:{}:*", server_id.simple(), user_id.simple());
        self.scan_and_delete(pattern, "user_lane", Some(*user_id), server_id)
            .await;
    }

    /// Wipe every cached response for `server_id` across **all** users
    /// and account labels.
    ///
    /// Called when an admin mutates server configuration that changes
    /// the upstream identity or wire shape: `endpoint_url`, transport,
    /// OAuth client/endpoint config, custom headers. Existing entries
    /// were minted against the *previous* upstream — leaving them in
    /// place would tunnel pre-update responses (potentially from a
    /// different provider, schema, or auth realm) into the new epoch
    /// until TTL expires.
    pub async fn invalidate_server_lane(&self, server_id: &Uuid) {
        let pattern = format!("{KEY_PREFIX}{}:*", server_id.simple());
        self.scan_and_delete(pattern, "server_lane", None, server_id)
            .await;
    }

    async fn scan_and_delete(
        &self,
        pattern: String,
        scope: &'static str,
        user_id: Option<Uuid>,
        server_id: &Uuid,
    ) {
        let mut cursor: String = "0".to_string();
        let mut deleted: usize = 0;
        loop {
            let page: Result<(String, Vec<String>), _> = self
                .redis
                .scan_page(
                    cursor.clone(),
                    pattern.clone(),
                    Some(256),
                    Some(ScanType::String),
                )
                .await;
            let (next, keys) = match page {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        server = %server_id, user = ?user_id, scope, error = %e,
                        "MCP cache invalidate: SCAN failed; some stale entries may persist"
                    );
                    return;
                }
            };
            if !keys.is_empty() {
                let n: Result<u64, _> = self.redis.del(keys.clone()).await;
                match n {
                    Ok(n) => deleted += n as usize,
                    Err(e) => tracing::warn!(error = %e, scope, "MCP cache invalidate: DEL failed"),
                }
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        if deleted > 0 {
            tracing::info!(
                server = %server_id,
                user = ?user_id,
                scope,
                deleted,
                "MCP cache invalidated"
            );
        }
        metrics::counter!("mcp_cache_invalidate_total", "scope" => scope).increment(deleted as u64);
    }
}

/// Render `caller` as `(user_part, label_part)` segments for the cache
/// key. `_` is the placeholder for "absent dimension"; real values are
/// hex-encoded so `_` can never appear inside an encoded segment.
fn caller_key_parts(caller: Option<CallerScope<'_>>) -> (String, String) {
    match caller {
        None => ("_".to_string(), "_".to_string()),
        Some(c) => {
            let user_part = c.user_id.simple().to_string();
            let label_part = match c.account_label {
                None | Some("") => "_".to_string(),
                Some(s) => hex::encode(s.as_bytes()),
            };
            (user_part, label_part)
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
        // Multi-account: two GitHub credentials (personal vs work).
        // Keys must differ so cross-account responses can't collide.
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
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let none_label = McpResponseCache::cache_key(&sid, Some(caller(&uid, None)), &req);
        let empty_label = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some(""))), &req);
        assert_eq!(none_label, empty_label);
    }

    #[test]
    fn key_shape_is_prefix_extractable() {
        // The whole point of the redesign: invalidation can SCAN by a
        // server-+-user prefix. Verify the prefix is recognisable.
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "x");
        let key = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("work"))), &req);
        let expected_prefix = format!("mcp_cache:{}:{}:", sid.simple(), uid.simple());
        assert!(
            key.starts_with(&expected_prefix),
            "key {key} does not start with {expected_prefix}"
        );
    }

    #[test]
    fn label_with_colon_does_not_collide_with_a_different_label() {
        // Hex encoding of the label means raw `:` can't appear in the
        // segment, so "a:b" and "ab" or "a" + ":b" stay distinct.
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "x");
        let with_colon = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("a:b"))), &req);
        let without = McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("ab"))), &req);
        assert_ne!(with_colon, without);
    }

    #[test]
    fn label_underscore_does_not_collide_with_no_label() {
        // The literal label "_" must not collide with the absent-label
        // placeholder `_` segment. Hex encoding keeps them distinct
        // (`_` placeholder vs `5f` hex of `_`).
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "x");
        let no_label = McpResponseCache::cache_key(&sid, Some(caller(&uid, None)), &req);
        let underscore_label =
            McpResponseCache::cache_key(&sid, Some(caller(&uid, Some("_"))), &req);
        assert_ne!(no_label, underscore_label);
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

    #[test]
    fn caller_key_parts_global() {
        let (u, l) = caller_key_parts(None);
        assert_eq!(u, "_");
        assert_eq!(l, "_");
    }

    #[test]
    fn caller_key_parts_per_user_no_label() {
        let uid = Uuid::new_v4();
        let (u, l) = caller_key_parts(Some(caller(&uid, None)));
        assert_eq!(u, uid.simple().to_string());
        assert_eq!(l, "_");
    }

    #[test]
    fn caller_key_parts_per_credential() {
        let uid = Uuid::new_v4();
        let (u, l) = caller_key_parts(Some(caller(&uid, Some("work"))));
        assert_eq!(u, uid.simple().to_string());
        assert_eq!(l, hex::encode("work"));
    }
}
