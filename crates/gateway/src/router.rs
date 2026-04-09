use crate::providers::DynAiProvider;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// One row in the router's model → provider table. The trait object
/// dispatches the actual upstream call; the UUID is the provider's
/// `providers.id` so the rate-limit engine can scope quotas per
/// provider without re-querying the DB on the hot path.
type ProviderEntry = (Arc<dyn DynAiProvider>, Uuid);

/// Routes model names to AI provider implementations.
///
/// Supports exact-match routing (e.g. `"gpt-4o" -> OpenAiProvider`) and
/// prefix-match as a fallback (e.g. `"gpt-" -> OpenAiProvider`).
///
/// In addition to the trait-object route, the router stores the
/// `providers.id` UUID for every registered model so the rate-limit
/// engine can resolve `(model → provider_id)` at request time and
/// apply per-provider quotas.
pub struct ModelRouter {
    /// Exact model name -> (provider trait, provider DB UUID).
    providers: HashMap<String, ProviderEntry>,
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelRouter {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider for a given model pattern.
    ///
    /// The pattern is used for both exact and prefix matching.
    /// For example, registering `"gpt-4o"` will match the exact model name,
    /// and registering `"gpt-"` will match any model starting with `"gpt-"`.
    /// `provider_id` is the `providers.id` UUID this pattern belongs to —
    /// used by `provider_id_for` so the rate-limit engine can scope
    /// quotas per provider.
    pub fn register_provider(
        &mut self,
        model_pattern: &str,
        provider: Arc<dyn DynAiProvider>,
        provider_id: Uuid,
    ) {
        self.providers
            .insert(model_pattern.to_string(), (provider, provider_id));
    }

    /// Look up the provider for a given model name.
    ///
    /// First tries an exact match, then falls back to the longest prefix match.
    pub fn route(&self, model: &str) -> Option<Arc<dyn DynAiProvider>> {
        self.lookup(model).map(|(p, _)| Arc::clone(p))
    }

    /// Look up the provider DB id for a given model name. Same exact-
    /// then-longest-prefix lookup as `route`. Returns `None` when no
    /// pattern matches — the rate-limit engine treats that as
    /// "no provider scope for this request".
    pub fn provider_id_for(&self, model: &str) -> Option<Uuid> {
        self.lookup(model).map(|(_, id)| *id)
    }

    /// Shared lookup core for `route` and `provider_id_for`.
    fn lookup(&self, model: &str) -> Option<&ProviderEntry> {
        // Exact match.
        if let Some(entry) = self.providers.get(model) {
            return Some(entry);
        }

        // Prefix match — pick the longest matching prefix for specificity.
        let mut best_match: Option<(&str, &ProviderEntry)> = None;
        for (pattern, entry) in &self.providers {
            if model.starts_with(pattern.as_str()) {
                match best_match {
                    Some((current_best, _)) if pattern.len() > current_best.len() => {
                        best_match = Some((pattern.as_str(), entry));
                    }
                    None => {
                        best_match = Some((pattern.as_str(), entry));
                    }
                    _ => {}
                }
            }
        }
        best_match.map(|(_, entry)| entry)
    }

    /// List all registered model patterns.
    pub fn list_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self.providers.keys().cloned().collect();
        models.sort();
        models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::*;
    use futures::Stream;
    use std::pin::Pin;

    struct DummyProvider {
        provider_name: String,
    }

    impl AiProvider for DummyProvider {
        fn name(&self) -> &str {
            &self.provider_name
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

    #[test]
    fn exact_match() {
        let mut router = ModelRouter::new();
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "openai".into(),
        });
        router.register_provider("gpt-4o", provider, Uuid::nil());

        let found = router.route("gpt-4o");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name(), "openai");
    }

    #[test]
    fn prefix_match() {
        let mut router = ModelRouter::new();
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "openai".into(),
        });
        router.register_provider("gpt-", provider, Uuid::nil());

        let found = router.route("gpt-4o-mini");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name(), "openai");
    }

    #[test]
    fn no_match() {
        let router = ModelRouter::new();
        assert!(router.route("unknown-model").is_none());
    }

    #[test]
    fn longest_prefix_wins() {
        let mut router = ModelRouter::new();
        let generic: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "generic".into(),
        });
        let specific: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "specific".into(),
        });
        router.register_provider("gpt-", generic, Uuid::nil());
        router.register_provider("gpt-4o", specific, Uuid::nil());

        let found = router.route("gpt-4o-mini");
        assert!(found.is_some());
        // "gpt-4o" is a longer prefix than "gpt-" for "gpt-4o-mini"
        assert_eq!(found.unwrap().name(), "specific");
    }

    #[test]
    fn provider_id_lookup() {
        let mut router = ModelRouter::new();
        let openai_id = Uuid::new_v4();
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "openai".into(),
        });
        router.register_provider("gpt-4o", provider, openai_id);
        assert_eq!(router.provider_id_for("gpt-4o"), Some(openai_id));
        assert_eq!(router.provider_id_for("unknown"), None);
    }
}
