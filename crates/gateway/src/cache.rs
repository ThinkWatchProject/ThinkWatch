use fred::clients::Client;
use fred::interfaces::KeysInterface;
use serde_json::Value;
use std::sync::Arc;
use think_watch_common::dynamic_config::DynamicConfig;
use xxhash_rust::xxh3::xxh3_128;

/// A cached answer, in the caller's format with PII placeholders intact.
pub struct Cached {
    pub body: Vec<u8>,
    /// Kept so a hit can debit quota the way the original call did.
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// What goes into Redis. The body is kept as JSON rather than bytes so
/// the entry stays readable with `redis-cli`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
    body: Value,
    prompt_tokens: u32,
    completion_tokens: u32,
}

/// Redis-based exact-match cache for LLM responses.
///
/// Only caches non-streaming requests with deterministic parameters
/// (temperature == 0 or absent).
///
/// Cache keys are purely semantic — see [`ResponseCache::fingerprint`]. All
/// users share the same cache — identical requests get the same
/// response regardless of who asked, which is correct since the
/// information surface is identical.
#[derive(Clone)]
pub struct ResponseCache {
    redis: Client,
    /// Source of the default TTL — read live on each `set` so admin
    /// edits to `gateway.cache_ttl_secs` take effect on the next
    /// cache write without a process restart. Previously this was a
    /// `u64` captured at boot, which silently ignored later edits
    /// while the sibling `mcp.cache_ttl_secs` setting was already
    /// read per-request. `Option` so tests can construct a cache
    /// without spinning up a DynamicConfig.
    ttl_source: TtlSource,
}

/// Either a live config handle or a fixed value. Production wires
/// the live handle; tests and the `with_default_ttl` constructor
/// pin a constant.
#[derive(Clone)]
enum TtlSource {
    Dynamic(Arc<DynamicConfig>),
    Fixed(u64),
}

impl ResponseCache {
    pub fn new(redis: Client, dynamic_config: Arc<DynamicConfig>) -> Self {
        Self {
            redis,
            ttl_source: TtlSource::Dynamic(dynamic_config),
        }
    }

    /// Create a cache with the default 1-hour TTL. Used by tests and
    /// any caller that doesn't have a `DynamicConfig` to thread in.
    pub fn with_default_ttl(redis: Client) -> Self {
        Self {
            redis,
            ttl_source: TtlSource::Fixed(3600),
        }
    }

    async fn default_ttl(&self) -> u64 {
        match &self.ttl_source {
            TtlSource::Dynamic(dc) => dc.cache_ttl_secs().await,
            TtlSource::Fixed(v) => *v,
        }
    }

    /// Whether this request is cacheable (deterministic): temperature
    /// absent or zero. A streamed request is eligible — the pump assembles
    /// the whole answer, and a later streamed hit replays it as one event.
    fn is_cacheable(request: &Value) -> bool {
        match request.get("temperature").and_then(Value::as_f64) {
            Some(t) => t == 0.0,
            None => true,
        }
    }

    /// The Redis key for a fingerprint.
    pub fn cache_key_for(fingerprint: &[u8]) -> String {
        // xxh3_128 is ~10x faster than SHA-256 for non-cryptographic hashing
        let hash = xxh3_128(fingerprint);
        format!("llm_cache:{hash:032x}")
    }

    /// The bytes that identify a request, or `None` when it must not be
    /// cached at all.
    ///
    /// **The whole request, not a chosen subset.** The key used to be
    /// model + messages + max_tokens, which silently ignored everything
    /// else that changes the answer: two requests differing only in
    /// `tools` shared a slot, and the second got the first one's tool
    /// call. It stayed hidden only because tools never reached an
    /// upstream. Hashing every field cannot forget one.
    ///
    /// **Cacheability is decided here, not at the lookup.** A request
    /// sampled at a nonzero temperature asks for a fresh draw. That check
    /// used to open `get` and `set`, where a refactor dropped it once;
    /// with no fingerprint there is no key to look up or store under.
    ///
    /// **Computed on the redacted request, on purpose.** What is stored
    /// carries placeholders and each caller restores their own values on
    /// the way out, so two callers asking the same question about their
    /// own e-mail share one slot — the point of a semantic cache, not a
    /// leak.
    ///
    /// `serde_json` sorts object keys when serializing, so the same
    /// request always produces the same bytes.
    pub fn fingerprint(request: &Value) -> Option<Vec<u8>> {
        if !Self::is_cacheable(request) {
            return None;
        }
        let mut r = request.clone();
        if let Some(obj) = r.as_object_mut() {
            // Framing, not the answer: a streamed request should hit
            // what a whole one stored.
            obj.remove("stream");
            obj.remove("stream_options");
        }
        Some(serde_json::to_vec(&r).unwrap_or_default())
    }

    /// Look up a cached answer.
    pub async fn get(&self, fingerprint: &[u8]) -> Option<Cached> {
        let key = Self::cache_key_for(fingerprint);
        let stored: Option<String> = self.redis.get(&key).await.ok().flatten();
        stored.and_then(|json| {
            serde_json::from_str::<Stored>(&json)
                .map_err(|e| tracing::warn!("Failed to read a cached response: {e}"))
                .ok()
                .map(|s| Cached {
                    body: serde_json::to_vec(&s.body).unwrap_or_default(),
                    prompt_tokens: s.prompt_tokens,
                    completion_tokens: s.completion_tokens,
                })
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

    /// Store an answer under the request's fingerprint.
    pub async fn set(&self, fingerprint: &[u8], cached: &Cached, ttl: Option<u64>) {
        let key = Self::cache_key_for(fingerprint);
        let ttl_secs = match ttl {
            Some(v) => v,
            None => self.default_ttl().await,
        };

        let Ok(body) = serde_json::from_slice::<Value>(&cached.body) else {
            // An answer that is not JSON is not worth replaying.
            return;
        };
        let json = match serde_json::to_string(&Stored {
            body,
            prompt_tokens: cached.prompt_tokens,
            completion_tokens: cached.completion_tokens,
        }) {
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
    use serde_json::json;

    fn req(model: &str, text: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": text}]})
    }

    fn key(r: &Value) -> String {
        ResponseCache::cache_key_for(&ResponseCache::fingerprint(r).expect("cacheable"))
    }

    #[test]
    fn the_same_request_always_produces_the_same_key() {
        let r = req("gpt-4o", "What is 2+2?");
        assert_eq!(key(&r), key(&r));
    }

    #[test]
    fn key_order_in_the_body_does_not_matter() {
        let a: Value = serde_json::from_str(r#"{"model":"m","messages":[],"top_p":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"top_p":1,"messages":[],"model":"m"}"#).unwrap();
        assert_eq!(key(&a), key(&b));
    }

    #[test]
    fn different_models_produce_different_keys() {
        assert_ne!(
            key(&req("gpt-4o", "ping")),
            key(&req("gpt-4o-mini", "ping"))
        );
    }

    #[test]
    fn different_prompts_produce_different_keys() {
        assert_ne!(key(&req("gpt-4o", "a")), key(&req("gpt-4o", "b")));
    }

    #[test]
    fn different_tools_produce_different_keys() {
        // The reason this was rewritten. The old key covered model +
        // messages + max_tokens, so "same question, different tools"
        // shared a slot and the second caller got the first one's tool
        // call. Hidden only while tools never reached an upstream.
        let mut a = req("gpt-4o", "do it");
        a["tools"] = json!([{"type":"function","function":{"name":"submit","parameters":{}}}]);
        let mut b = req("gpt-4o", "do it");
        b["tools"] = json!([{"type":"function","function":{"name":"cancel","parameters":{}}}]);
        assert_ne!(key(&a), key(&b));
    }

    #[test]
    fn any_field_the_caller_sent_is_in_the_key() {
        let base = req("gpt-4o", "x");
        for (field, value) in [
            ("top_p", json!(0.5)),
            ("stop", json!(["END"])),
            ("seed", json!(7)),
            ("response_format", json!({"type": "json_object"})),
        ] {
            let mut v = base.clone();
            v[field] = value;
            assert_ne!(key(&base), key(&v), "{field} is not in the key");
        }
    }

    #[test]
    fn streaming_does_not_change_the_key() {
        let mut a = req("gpt-4o", "x");
        a["stream"] = json!(true);
        a["stream_options"] = json!({"include_usage": true});
        assert_eq!(key(&a), key(&req("gpt-4o", "x")));
    }

    #[test]
    fn a_nonzero_temperature_has_no_fingerprint_so_it_can_never_be_looked_up() {
        // This gate used to live inside get/set and a refactor dropped it
        // once. A nonzero temperature asks for a fresh draw.
        let mut r = req("gpt-4o", "x");
        r["temperature"] = json!(0.7);
        assert!(ResponseCache::fingerprint(&r).is_none());
        r["temperature"] = json!(0.0);
        assert!(ResponseCache::fingerprint(&r).is_some());
    }

    #[test]
    fn keys_carry_their_prefix() {
        assert!(key(&req("gpt-4o", "x")).starts_with("llm_cache:"));
    }
}
