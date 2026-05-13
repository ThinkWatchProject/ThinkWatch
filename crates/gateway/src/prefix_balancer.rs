use crate::providers::DynAiProvider;
use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
};
use futures::Stream;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Routes requests with similar prompt prefixes to the same backend
/// to maximize KV cache reuse in self-hosted LLM scenarios (vLLM, TGI).
pub struct PrefixBalancer {
    /// Maps prompt prefix hash → backend index.
    prefix_map: RwLock<HashMap<u64, usize>>,
    backends: Vec<Arc<dyn DynAiProvider>>,
    /// Number of characters to use for prefix hashing.
    prefix_length: usize,
}

impl PrefixBalancer {
    pub fn new(backends: Vec<Arc<dyn DynAiProvider>>, prefix_length: usize) -> Self {
        Self {
            prefix_map: RwLock::new(HashMap::new()),
            backends,
            prefix_length,
        }
    }

    /// Extract the first `prefix_length` characters from the first user message.
    fn extract_prefix(&self, request: &ChatCompletionRequest) -> Option<String> {
        for msg in &request.messages {
            if msg.role == "user" {
                let text = match &msg.content {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(parts) => {
                        let mut combined = String::new();
                        for part in parts {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                combined.push_str(t);
                            }
                        }
                        combined
                    }
                    _ => continue,
                };

                if text.is_empty() {
                    continue;
                }

                // Take first prefix_length characters
                let prefix: String = text.chars().take(self.prefix_length).collect();
                return Some(prefix);
            }
        }
        None
    }

    /// Hash a prefix string using the standard hasher.
    fn hash_prefix(prefix: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        prefix.hash(&mut hasher);
        hasher.finish()
    }

    /// Select the backend index for a request.
    async fn select_backend(&self, request: &ChatCompletionRequest) -> usize {
        let len = self.backends.len();
        if len == 0 {
            return 0;
        }

        let prefix = match self.extract_prefix(request) {
            Some(p) => p,
            None => return 0, // No user message — use first backend
        };

        let hash = Self::hash_prefix(&prefix);

        // Check if we already have a mapping for this prefix
        {
            let map = self.prefix_map.read().await;
            if let Some(&idx) = map.get(&hash)
                && idx < len
            {
                return idx;
            }
        }

        // Consistent hash: assign to backend based on hash
        let idx = (hash as usize) % len;

        // Store the mapping
        {
            let mut map = self.prefix_map.write().await;
            map.insert(hash, idx);
        }

        idx
    }
}

impl DynAiProvider for PrefixBalancer {
    fn name(&self) -> &str {
        "prefix_balancer"
    }

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            if self.backends.is_empty() {
                return Err(GatewayError::ProviderError(
                    "No backends configured for prefix balancer".into(),
                ));
            }

            let idx = self.select_backend(&request).await;
            let len = self.backends.len();

            // Try selected backend first, then fall through to others
            for attempt in 0..len {
                let backend_idx = (idx + attempt) % len;
                let backend = &self.backends[backend_idx];

                match backend.chat_completion_boxed(request.clone()).await {
                    Ok(resp) => return Ok(resp),
                    Err(e) if attempt + 1 < len => {
                        tracing::warn!(
                            backend = backend.name(),
                            attempt,
                            "Prefix balancer backend failed, trying next: {e}"
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }

            Err(GatewayError::ProviderError(
                "All prefix balancer backends failed".into(),
            ))
        })
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        if self.backends.is_empty() {
            return Box::pin(futures::stream::once(async {
                Err(GatewayError::ProviderError(
                    "No backends configured for prefix balancer".into(),
                ))
            }));
        }

        // For streaming, we need to synchronously pick a backend.
        // Use the hash directly without async prefix_map lookup.
        let prefix = self.extract_prefix(&request);
        let idx = match prefix {
            Some(p) => {
                let hash = Self::hash_prefix(&p);
                (hash as usize) % self.backends.len()
            }
            None => 0,
        };

        self.backends[idx].stream_chat_completion(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatCompletionChunk, ChatMessage};

    struct DummyProvider {
        provider_name: &'static str,
    }

    impl crate::providers::traits::AiProvider for DummyProvider {
        fn name(&self) -> &str {
            self.provider_name
        }

        async fn chat_completion(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            Err(GatewayError::ProviderError("dummy".into()))
        }

        fn stream_chat_completion(
            &self,
            _request: ChatCompletionRequest,
        ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
            Box::pin(futures::stream::empty())
        }
    }

    fn balancer(backends: usize, prefix_length: usize) -> PrefixBalancer {
        let providers: Vec<Arc<dyn DynAiProvider>> = (0..backends)
            .map(|i| {
                let name = Box::leak(format!("p{i}").into_boxed_str()) as &'static str;
                Arc::new(DummyProvider { provider_name: name }) as Arc<dyn DynAiProvider>
            })
            .collect();
        PrefixBalancer::new(providers, prefix_length)
    }

    fn req(messages: Vec<ChatMessage>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "m".into(),
            messages,
            temperature: None,
            max_tokens: None,
            stream: None,
            extra: serde_json::Value::Null,
            caller_user_id: None,
            caller_user_email: None,
            trace_id: None,
        }
    }

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: serde_json::Value::String(content.into()),
        }
    }

    fn system_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "system".into(),
            content: serde_json::Value::String(content.into()),
        }
    }

    #[test]
    fn extract_prefix_takes_first_user_message() {
        let b = balancer(2, 20);
        let r = req(vec![
            system_msg("You are a helpful assistant"),
            user_msg("Hello world"),
        ]);
        assert_eq!(b.extract_prefix(&r), Some("Hello world".into()));
    }

    #[test]
    fn extract_prefix_truncates_to_prefix_length() {
        let b = balancer(2, 5);
        let r = req(vec![user_msg("Hello world this is long")]);
        assert_eq!(b.extract_prefix(&r), Some("Hello".into()));
    }

    #[test]
    fn extract_prefix_concatenates_array_content_text_parts() {
        // OpenAI-style multimodal: content is an array of {type, text}/{type, image_url}.
        // Only the text parts contribute to the prefix — images are skipped.
        let b = balancer(2, 100);
        let r = req(vec![ChatMessage {
            role: "user".into(),
            content: serde_json::json!([
                {"type": "text", "text": "Part one. "},
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "Part two."},
            ]),
        }]);
        assert_eq!(b.extract_prefix(&r), Some("Part one. Part two.".into()));
    }

    #[test]
    fn extract_prefix_skips_empty_user_message_then_takes_next() {
        // Defensive: an empty user message followed by a real one (e.g. caller
        // building up history) should still produce the real prefix.
        let b = balancer(2, 50);
        let r = req(vec![
            ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String(String::new()),
            },
            user_msg("real question"),
        ]);
        assert_eq!(b.extract_prefix(&r), Some("real question".into()));
    }

    #[test]
    fn extract_prefix_returns_none_when_no_user_message() {
        let b = balancer(2, 50);
        let r = req(vec![system_msg("just a system prompt")]);
        assert_eq!(b.extract_prefix(&r), None);
    }

    #[test]
    fn extract_prefix_handles_unicode_char_boundary() {
        // `.chars().take(N)` not `[..N]` — locks in that we count graphemes,
        // not bytes, so multi-byte chars don't panic at a non-boundary cut.
        let b = balancer(2, 3);
        let r = req(vec![user_msg("你好世界")]);
        assert_eq!(b.extract_prefix(&r), Some("你好世".into()));
    }

    #[tokio::test]
    async fn select_backend_is_sticky_for_same_prefix() {
        // The first selection writes the mapping; every subsequent call
        // with the same prefix MUST return the same backend, otherwise
        // the KV-cache-affinity rationale collapses.
        let b = balancer(4, 10);
        let r = req(vec![user_msg("system prompt v1")]);
        let first = b.select_backend(&r).await;
        for _ in 0..20 {
            assert_eq!(b.select_backend(&r).await, first);
        }
    }

    #[tokio::test]
    async fn select_backend_returns_zero_when_no_backends() {
        // Defensive default — no backends means there's nothing to pick;
        // returning 0 here matches the empty-request behavior so the caller
        // hits the same `backends.is_empty()` guard in chat_completion_boxed.
        let b = balancer(0, 10);
        let r = req(vec![user_msg("anything")]);
        assert_eq!(b.select_backend(&r).await, 0);
    }

    #[tokio::test]
    async fn select_backend_indexes_within_bounds() {
        // Hash mod len must always produce a valid index. Sweep a bunch
        // of different prefixes to make sure no path returns >= len.
        let b = balancer(3, 10);
        for prompt in [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta",
        ] {
            let r = req(vec![user_msg(prompt)]);
            let idx = b.select_backend(&r).await;
            assert!(idx < 3, "{prompt} → idx {idx} out of bounds for len 3");
        }
    }

    #[tokio::test]
    async fn select_backend_returns_zero_for_no_user_message() {
        let b = balancer(3, 10);
        let r = req(vec![system_msg("only a system prompt")]);
        assert_eq!(b.select_backend(&r).await, 0);
    }
}
