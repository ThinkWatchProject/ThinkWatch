use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
};
use crate::providers::DynAiProvider;
use futures::Stream;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
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
            if let Some(&idx) = map.get(&hash) {
                if idx < len {
                    return idx;
                }
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
