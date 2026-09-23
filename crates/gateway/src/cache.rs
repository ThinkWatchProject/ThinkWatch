use crate::providers::traits::{ChatCompletionRequest, ChatCompletionResponse};
use fred::clients::Client;
use fred::interfaces::KeysInterface;
use std::sync::Arc;
use think_watch_common::dynamic_config::DynamicConfig;
use xxhash_rust::xxh3::xxh3_128;

/// Redis-based exact-match cache for LLM responses.
///
/// Only caches non-streaming requests with deterministic parameters
/// (temperature == 0 or absent).
///
/// Cache keys are purely semantic: `model + messages + params`. All
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

    /// Whether this request is cacheable (deterministic).
    ///
    /// Both streaming and non-streaming requests are eligible — for
    /// streaming the proxy assembles the complete response from chunks
    /// after the stream ends and writes it to cache as a normal
    /// `ChatCompletionResponse`.  On a subsequent cache hit with
    /// `stream=true`, the assembled response is re-emitted as a
    /// single-chunk SSE stream.
    pub fn is_cacheable(request: &ChatCompletionRequest) -> bool {
        // Only cache when temperature is 0 or absent
        match request.temperature {
            Some(t) => t == 0.0,
            None => true,
        }
    }

    /// Compute the cache key for a request.
    ///
    /// **The fingerprint is the upstream request itself.** Earlier this
    /// hashed a hand-picked triple — model, messages, max_tokens — which
    /// silently ignored everything else that changes the answer. Two
    /// requests with the same messages and different `tools` produced the
    /// same key, and the second one got the first one's tool call. That
    /// was survivable only because tools never reached an upstream at
    /// all; fixing the conversion layer would have turned it into served
    /// wrong answers.
    ///
    /// Encoding the intermediate representation cannot miss a field by
    /// construction: whatever the upstream is going to be asked is what
    /// gets hashed.
    ///
    /// **Computed after redaction, on purpose.** What is stored carries
    /// placeholders (`{{EMAIL_1}}`), and restoration happens on the way
    /// out using *this* caller's context. So two callers asking the same
    /// question with their own e-mail addresses share one slot and each
    /// gets their own value back — that is the point of a semantic
    /// cache, not a leak.
    pub fn cache_key_for(fingerprint: &[u8]) -> String {
        // xxh3_128 is ~10x faster than SHA-256 for non-cryptographic hashing
        let hash = xxh3_128(fingerprint);
        format!("llm_cache:{hash:032x}")
    }

    /// The bytes that identify a request.
    ///
    /// The whole request, not a chosen subset — `extra` is flattened, so
    /// `tools`, `tool_choice` and every other field the caller sent are
    /// in here by construction. That is the point: the previous key was
    /// three hand-picked fields, and a field nobody remembered to add
    /// was a silent collision.
    pub fn fingerprint(request: &ChatCompletionRequest) -> Vec<u8> {
        let mut r = request.clone();
        // Streaming changes the framing, not the answer, so a streaming
        // request should hit what a buffered one stored.
        r.stream = None;
        serde_json::to_vec(&r).unwrap_or_default()
    }

    /// Look up a cached response.
    ///
    /// `fingerprint` comes from [`Cache::fingerprint`], computed before
    /// redaction — see [`Cache::cache_key_for`] for why that ordering is
    /// not optional.
    pub async fn get(&self, fingerprint: &[u8]) -> Option<ChatCompletionResponse> {
        let key = Self::cache_key_for(fingerprint);
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
    /// Store a response under the request's fingerprint.
    pub async fn set(
        &self,
        fingerprint: &[u8],
        response: &ChatCompletionResponse,
        ttl: Option<u64>,
    ) {
        let key = Self::cache_key_for(fingerprint);
        let ttl_secs = match ttl {
            Some(v) => v,
            None => self.default_ttl().await,
        };

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

    fn req(model: &str, text: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String(text.into()),
                ..Default::default()
            }],
            temperature: None,
            max_tokens: None,
            stream: None,
            extra: serde_json::json!({}),
        }
    }

    fn key(r: &ChatCompletionRequest) -> String {
        ResponseCache::cache_key_for(&ResponseCache::fingerprint(r))
    }

    #[test]
    fn the_same_request_always_produces_the_same_key() {
        let r = req("gpt-4o", "What is 2+2?");
        assert_eq!(key(&r), key(&r));
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
        // 这条是这次改写的理由。旧 key 只覆盖 model + messages +
        // max_tokens，于是「同样的问题，不同的工具」撞进同一个槽，
        // 第二个请求拿到第一个的工具调用。
        //
        // 以前碰不上，是因为工具根本到不了上游（见 core 的 issue #50）——
        // 把转换修好，它就会变成实实在在的错答案。
        let tools = |name: &str| {
            serde_json::json!({ "tools": [{
                "type": "function",
                "function": { "name": name, "parameters": { "type": "object" } }
            }]})
        };
        let mut a = req("gpt-4o", "do it");
        a.extra = tools("submit");
        let mut b = req("gpt-4o", "do it");
        b.extra = tools("cancel");
        assert_ne!(key(&a), key(&b), "工具不同，答案就不同");
    }

    #[test]
    fn any_field_the_caller_sent_is_in_the_key() {
        // 旧 key 是手挑的三个字段，漏掉的都是撞槽的来源。
        // 现在整份请求都进指纹，漏不掉
        let base = req("gpt-4o", "x");
        for extra in [
            serde_json::json!({ "top_p": 0.5 }),
            serde_json::json!({ "stop": ["END"] }),
            serde_json::json!({ "seed": 7 }),
            serde_json::json!({ "response_format": { "type": "json_object" } }),
        ] {
            let mut v = base.clone();
            v.extra = extra.clone();
            assert_ne!(key(&base), key(&v), "{extra} 没进 key");
        }
    }

    #[test]
    fn streaming_does_not_change_the_key() {
        // 流式改的是分帧，不是答案。流式请求该命中非流式存下的那份
        let mut a = req("gpt-4o", "x");
        a.stream = Some(true);
        assert_eq!(key(&a), key(&req("gpt-4o", "x")));
    }

    #[test]
    fn a_nonzero_temperature_is_not_cacheable() {
        let mut r = req("gpt-4o", "x");
        r.temperature = Some(0.7);
        assert!(!ResponseCache::is_cacheable(&r));
        r.temperature = Some(0.0);
        assert!(ResponseCache::is_cacheable(&r));
        r.temperature = None;
        assert!(ResponseCache::is_cacheable(&r));
    }

    #[test]
    fn keys_carry_their_prefix() {
        assert!(key(&req("gpt-4o", "x")).starts_with("llm_cache:"));
    }
}
