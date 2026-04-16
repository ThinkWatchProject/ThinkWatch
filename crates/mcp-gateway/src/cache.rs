use fred::clients::Client;
use fred::interfaces::KeysInterface;
use uuid::Uuid;
use xxhash_rust::xxh3::xxh3_128;

use crate::proxy::{JsonRpcRequest, JsonRpcResponse};

const KEY_PREFIX: &str = "mcp_cache:";

/// Redis-based exact-match cache for MCP tool call responses.
///
/// Cache keys are semantic: `server_id + method + params`, optionally
/// scoped by `user_id` when the upstream server forwards caller
/// identity.  This ensures that:
///
/// - **Shared servers** (no `{{user_id}}`/`{{user_email}}` headers):
///   all users share one cache entry — identical request → identical
///   response regardless of caller.
/// - **Per-user servers** (identity forwarded): each user gets their
///   own cache entry because the upstream may return different results
///   depending on who's calling.
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
    /// When `user_id` is `Some`, it is mixed into the hash so each user
    /// gets a separate cache lane.  Pass `None` for shared (user-agnostic)
    /// caching.
    pub fn cache_key(server_id: &Uuid, user_id: Option<&Uuid>, request: &JsonRpcRequest) -> String {
        let params_json = request
            .params
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap_or_default())
            .unwrap_or_default();

        let mut input = Vec::with_capacity(256);
        input.extend_from_slice(server_id.as_bytes());
        if let Some(uid) = user_id {
            input.push(b':');
            input.extend_from_slice(uid.as_bytes());
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
        user_id: Option<&Uuid>,
        request: &JsonRpcRequest,
    ) -> Option<JsonRpcResponse> {
        let key = Self::cache_key(server_id, user_id, request);
        let cached: Option<String> = self.redis.get(&key).await.ok().flatten();
        cached.and_then(|json| {
            serde_json::from_str::<JsonRpcResponse>(&json)
                .map_err(|e| {
                    tracing::warn!("Failed to deserialize cached MCP response: {e}");
                    e
                })
                .ok()
        })
    }

    /// Store a response in the cache with the given TTL (in seconds).
    pub async fn set(
        &self,
        server_id: &Uuid,
        user_id: Option<&Uuid>,
        request: &JsonRpcRequest,
        response: &JsonRpcResponse,
        ttl_secs: u64,
    ) {
        // Don't cache error responses.
        if response.error.is_some() {
            return;
        }

        let key = Self::cache_key(server_id, user_id, request);
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

        if let Err(e) = result {
            tracing::warn!("Failed to cache MCP response: {e}");
        }
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
    fn user_scoped_key_differs_from_shared() {
        let sid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let shared = McpResponseCache::cache_key(&sid, None, &req);
        let scoped = McpResponseCache::cache_key(&sid, Some(&uid), &req);
        assert_ne!(shared, scoped);
    }

    #[test]
    fn different_users_produce_different_keys() {
        let sid = Uuid::new_v4();
        let req = make_request("tools/call", "query");
        let k1 = McpResponseCache::cache_key(&sid, Some(&Uuid::new_v4()), &req);
        let k2 = McpResponseCache::cache_key(&sid, Some(&Uuid::new_v4()), &req);
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_has_expected_prefix() {
        let key =
            McpResponseCache::cache_key(&Uuid::new_v4(), None, &make_request("tools/call", "x"));
        assert!(key.starts_with("mcp_cache:"), "got {key}");
    }
}
