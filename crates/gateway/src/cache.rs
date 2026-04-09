use crate::providers::traits::{ChatCompletionRequest, ChatCompletionResponse};
use fred::clients::Client;
use fred::interfaces::KeysInterface;
use xxhash_rust::xxh3::xxh3_128;

/// Redis-based exact-match cache for LLM responses.
///
/// Only caches non-streaming requests with deterministic parameters
/// (temperature == 0 or absent).
///
/// **Scope is mandatory.** The previous design used a global key
/// space — same prompt from different users → same cache hit. That
/// leaked confidential prompts across tenants ("what's my salary?"
/// from User A returns User B's cached answer). The `scope` argument
/// on `get`/`set` is now required and is mixed into the hash key.
/// Callers should pass the API key id (or, when present, the user id)
/// so two clients can never collide.
#[derive(Clone)]
pub struct ResponseCache {
    redis: Client,
    /// Default TTL in seconds for cached entries.
    default_ttl: u64,
}

impl ResponseCache {
    pub fn new(redis: Client, default_ttl: u64) -> Self {
        Self { redis, default_ttl }
    }

    /// Create a cache with the default 1-hour TTL.
    pub fn with_default_ttl(redis: Client) -> Self {
        Self::new(redis, 3600)
    }

    /// Whether this request is cacheable (deterministic).
    pub fn is_cacheable(request: &ChatCompletionRequest) -> bool {
        // Don't cache streaming requests
        if request.stream.unwrap_or(false) {
            return false;
        }
        // Only cache when temperature is 0 or absent
        match request.temperature {
            Some(t) => t == 0.0,
            None => true,
        }
    }

    /// Compute the cache key for a request, scoped to a tenant.
    /// The scope is mixed into the hash so two callers can never
    /// share a key — see the type-level note for the rationale.
    pub fn cache_key(request: &ChatCompletionRequest, scope: &str) -> String {
        // Normalize messages to deterministic JSON for hashing
        let messages_json = serde_json::to_string(&request.messages).unwrap_or_default();

        // Build the input bytes for hashing
        let mut input = Vec::with_capacity(256);
        input.extend_from_slice(scope.as_bytes());
        input.push(b':');
        input.extend_from_slice(request.model.as_bytes());
        input.push(b':');
        input.extend_from_slice(messages_json.as_bytes());
        if let Some(mt) = request.max_tokens {
            input.extend_from_slice(b":mt=");
            input.extend_from_slice(mt.to_string().as_bytes());
        }

        // xxh3_128 is ~10x faster than SHA-256 for non-cryptographic hashing
        let hash = xxh3_128(&input);
        format!("llm_cache:{hash:032x}")
    }

    /// Look up a cached response. `scope` MUST identify the
    /// requesting tenant (api_key id or user id) so caches can't
    /// cross tenant boundaries.
    pub async fn get(
        &self,
        request: &ChatCompletionRequest,
        scope: &str,
    ) -> Option<ChatCompletionResponse> {
        if !Self::is_cacheable(request) {
            return None;
        }

        let key = Self::cache_key(request, scope);

        let cached: Option<String> = self.redis.get(&key).await.ok().flatten();

        cached.and_then(|json| {
            serde_json::from_str::<ChatCompletionResponse>(&json)
                .map_err(|e| {
                    tracing::warn!("Failed to deserialize cached response: {e}");
                    e
                })
                .ok()
        })
    }

    /// Invalidate all cached responses by deleting keys matching the cache prefix.
    /// Uses Lua script for atomic pattern deletion.
    pub async fn invalidate_all(&self) {
        use fred::interfaces::LuaInterface;
        // Use Lua EVAL to scan and delete in batches server-side
        const LUA_INVALIDATE: &str = r#"
local cursor = '0'
local total = 0
repeat
    local result = redis.call('SCAN', cursor, 'MATCH', ARGV[1], 'COUNT', 100)
    cursor = result[1]
    local keys = result[2]
    if #keys > 0 then
        redis.call('DEL', unpack(keys))
        total = total + #keys
    end
until cursor == '0'
return total
"#;
        let deleted: i64 = self
            .redis
            .eval(
                LUA_INVALIDATE,
                Vec::<String>::new(),
                vec!["llm_cache:*".to_string()],
            )
            .await
            .unwrap_or(0);
        metrics::counter!("gateway_cache_invalidations_total").increment(1);
        tracing::info!(deleted, "Cache invalidated");
    }

    /// Store a response in the cache. `scope` MUST identify the
    /// requesting tenant — see `get` for the contract.
    pub async fn set(
        &self,
        request: &ChatCompletionRequest,
        scope: &str,
        response: &ChatCompletionResponse,
        ttl: Option<u64>,
    ) {
        if !Self::is_cacheable(request) {
            return;
        }

        let key = Self::cache_key(request, scope);
        let ttl_secs = ttl.unwrap_or(self.default_ttl);

        let json = match serde_json::to_string(response) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to serialize response for cache: {e}");
                return;
            }
        };

        let expiration = fred::types::Expiration::EX(ttl_secs as i64);
        let result: Result<(), _> = self
            .redis
            .set(&key, json.as_str(), Some(expiration), None, false)
            .await;

        if let Err(e) = result {
            tracing::warn!("Failed to cache response: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::ChatMessage;

    fn req(model: &str, prompt: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(prompt.to_string()),
            }],
            temperature: Some(0.0),
            max_tokens: Some(1024),
            stream: None,
            extra: serde_json::json!({}),
        }
    }

    #[test]
    fn cache_key_is_deterministic_for_same_scope() {
        let r = req("gpt-4o", "What is 2+2?");
        let k1 = ResponseCache::cache_key(&r, "user-alice");
        let k2 = ResponseCache::cache_key(&r, "user-alice");
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_scopes_produce_different_keys() {
        // Same prompt, same model, different tenants → MUST collide
        // to different keys. This is the regression test for the
        // wave-2 cross-tenant cache leak.
        let r = req("gpt-4o", "What is my salary?");
        let alice = ResponseCache::cache_key(&r, "user-alice");
        let bob = ResponseCache::cache_key(&r, "user-bob");
        assert_ne!(
            alice, bob,
            "scoped cache keys must not collide across tenants"
        );
    }

    #[test]
    fn empty_scope_still_isolates() {
        // Even an empty scope is treated literally — it doesn't fall
        // back to the global key space. This catches a class of bug
        // where a caller forgot to populate scope and accidentally
        // shared a cache slot with every other empty-scope request.
        let r = req("gpt-4o", "ping");
        let empty = ResponseCache::cache_key(&r, "");
        let alice = ResponseCache::cache_key(&r, "user-alice");
        assert_ne!(empty, alice);
    }

    #[test]
    fn different_models_produce_different_keys() {
        let alice_4o = ResponseCache::cache_key(&req("gpt-4o", "ping"), "user-alice");
        let alice_5 = ResponseCache::cache_key(&req("gpt-5", "ping"), "user-alice");
        assert_ne!(alice_4o, alice_5);
    }

    #[test]
    fn different_messages_produce_different_keys() {
        let one = ResponseCache::cache_key(&req("gpt-4o", "hello"), "alice");
        let two = ResponseCache::cache_key(&req("gpt-4o", "world"), "alice");
        assert_ne!(one, two);
    }

    #[test]
    fn cache_key_has_expected_prefix() {
        // The Redis key prefix is what `invalidate_all` matches against,
        // so a typo here would silently break cache invalidation.
        let key = ResponseCache::cache_key(&req("gpt-4o", "ping"), "alice");
        assert!(key.starts_with("llm_cache:"), "got {key}");
    }

    #[test]
    fn streaming_requests_are_not_cacheable() {
        let mut r = req("gpt-4o", "ping");
        r.stream = Some(true);
        assert!(!ResponseCache::is_cacheable(&r));
    }

    #[test]
    fn high_temperature_requests_are_not_cacheable() {
        let mut r = req("gpt-4o", "ping");
        r.temperature = Some(0.7);
        assert!(!ResponseCache::is_cacheable(&r));
    }

    #[test]
    fn temperature_zero_is_cacheable() {
        let r = req("gpt-4o", "ping");
        assert!(ResponseCache::is_cacheable(&r));
    }
}
