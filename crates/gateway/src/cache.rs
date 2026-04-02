use crate::providers::traits::{ChatCompletionRequest, ChatCompletionResponse};
use fred::clients::Client;
use fred::interfaces::KeysInterface;
use sha2::{Digest, Sha256};

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

    /// Compute the cache key for a request.
    fn cache_key(request: &ChatCompletionRequest) -> String {
        // Normalize messages to deterministic JSON for hashing
        let messages_json = serde_json::to_string(&request.messages).unwrap_or_default();

        let mut hasher = Sha256::new();
        hasher.update(request.model.as_bytes());
        hasher.update(b":");
        hasher.update(messages_json.as_bytes());

        // Include max_tokens in the hash if set, since it affects output
        if let Some(mt) = request.max_tokens {
            hasher.update(b":mt=");
            hasher.update(mt.to_string().as_bytes());
        }

        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        format!("llm_cache:{hex}")
    }

    /// Look up a cached response.
    pub async fn get(
        &self,
        request: &ChatCompletionRequest,
    ) -> Option<ChatCompletionResponse> {
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
