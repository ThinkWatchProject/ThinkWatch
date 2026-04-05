use crate::providers::traits::{ChatCompletionRequest, ChatCompletionResponse};
use fred::clients::Client;
use fred::interfaces::KeysInterface;
use xxhash_rust::xxh3::xxh3_128;

/// Redis-based exact-match cache for LLM responses.
///
/// Only caches non-streaming requests with deterministic parameters
/// (temperature == 0 or absent).
///
/// NOTE: The cache is intentionally shared across all users. The cache key is
/// derived solely from the model, messages, and max_tokens. This is by design
/// for many use cases (identical prompts yield identical results when temperature
/// is 0). If per-user isolation is needed, callers should incorporate a user or
/// team identifier into the request before caching.
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

    /// Compute the cache key for a request, optionally scoped to a user/team.
    pub fn cache_key_with_scope(request: &ChatCompletionRequest, scope: Option<&str>) -> String {
        Self::cache_key_inner(request, scope)
    }

    /// Compute the cache key for a request (shared/global scope).
    fn cache_key(request: &ChatCompletionRequest) -> String {
        Self::cache_key_inner(request, None)
    }

    fn cache_key_inner(request: &ChatCompletionRequest, scope: Option<&str>) -> String {
        // Normalize messages to deterministic JSON for hashing
        let messages_json = serde_json::to_string(&request.messages).unwrap_or_default();

        // Build the input bytes for hashing
        let mut input = Vec::with_capacity(256);
        if let Some(s) = scope {
            input.extend_from_slice(s.as_bytes());
            input.push(b':');
        }
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

    /// Look up a cached response.
    pub async fn get(&self, request: &ChatCompletionRequest) -> Option<ChatCompletionResponse> {
        if !Self::is_cacheable(request) {
            return None;
        }

        let key = Self::cache_key(request);

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
            .eval(LUA_INVALIDATE, Vec::<String>::new(), vec!["llm_cache:*".to_string()])
            .await
            .unwrap_or(0);
        metrics::counter!("gateway_cache_invalidations_total").increment(1);
        tracing::info!(deleted, "Cache invalidated");
    }

    /// Store a response in the cache.
    pub async fn set(
        &self,
        request: &ChatCompletionRequest,
        response: &ChatCompletionResponse,
        ttl: Option<u64>,
    ) {
        if !Self::is_cacheable(request) {
            return;
        }

        let key = Self::cache_key(request);
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
