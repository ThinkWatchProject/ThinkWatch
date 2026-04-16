use crate::providers::DynAiProvider;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// A single route entry mapping a model to a provider with weight/priority.
pub struct RouteEntry {
    pub provider: Arc<dyn DynAiProvider>,
    pub provider_id: Uuid,
    pub upstream_model: Option<String>,
    pub weight: u32,
    pub priority: u32,
}

/// Routes model names to AI provider implementations with multi-provider
/// failover and weighted traffic splitting.
///
/// Each model can have multiple routes at different priority levels.
/// Within a priority group, routes are selected by weighted random.
/// If all routes in a group fail, the next priority group is tried.
///
/// Also supports prefix-match as a fallback (e.g. `"gpt-" -> OpenAiProvider`)
/// for providers that have no explicit model routes configured.
pub struct ModelRouter {
    /// Exact model name -> list of routes sorted by (priority ASC, weight DESC).
    routes: HashMap<String, Vec<RouteEntry>>,
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelRouter {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    /// Register a route for a given model.
    pub fn register_route(&mut self, model_id: &str, entry: RouteEntry) {
        self.routes
            .entry(model_id.to_string())
            .or_default()
            .push(entry);
    }

    /// Sort all route vecs by (priority ASC, weight DESC). Call once after
    /// all routes have been registered.
    pub fn sort_routes(&mut self) {
        for entries in self.routes.values_mut() {
            entries.sort_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| b.weight.cmp(&a.weight))
            });
        }
    }

    /// Look up all routes for a model (exact match then prefix fallback).
    /// Returns routes sorted by priority ASC, weight DESC.
    pub fn route(&self, model: &str) -> Option<&Vec<RouteEntry>> {
        // Exact match.
        if let Some(entries) = self.routes.get(model)
            && !entries.is_empty()
        {
            return Some(entries);
        }

        // Prefix match — pick the longest matching prefix for specificity.
        let mut best_match: Option<(&str, &Vec<RouteEntry>)> = None;
        for (pattern, entries) in &self.routes {
            if model.starts_with(pattern.as_str()) && !entries.is_empty() {
                match best_match {
                    Some((current_best, _)) if pattern.len() > current_best.len() => {
                        best_match = Some((pattern.as_str(), entries));
                    }
                    None => {
                        best_match = Some((pattern.as_str(), entries));
                    }
                    _ => {}
                }
            }
        }
        best_match.map(|(_, entries)| entries)
    }

    /// Look up the first provider DB id for a given model name.
    /// Used to scope rate-limit counters by provider.
    pub fn provider_id_for(&self, model: &str) -> Option<Uuid> {
        self.route(model)
            .and_then(|entries| entries.first())
            .map(|e| e.provider_id)
    }

    /// List all registered model patterns.
    pub fn list_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self.routes.keys().cloned().collect();
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
        router.register_route(
            "gpt-4o",
            RouteEntry {
                provider,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );

        let found = router.route("gpt-4o");
        assert!(found.is_some());
        assert_eq!(found.unwrap()[0].provider.name(), "openai");
    }

    #[test]
    fn prefix_match() {
        let mut router = ModelRouter::new();
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "openai".into(),
        });
        router.register_route(
            "gpt-",
            RouteEntry {
                provider,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );

        let found = router.route("gpt-4o-mini");
        assert!(found.is_some());
        assert_eq!(found.unwrap()[0].provider.name(), "openai");
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
        router.register_route(
            "gpt-",
            RouteEntry {
                provider: generic,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );
        router.register_route(
            "gpt-4o",
            RouteEntry {
                provider: specific,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );

        let found = router.route("gpt-4o-mini");
        assert!(found.is_some());
        // "gpt-4o" is a longer prefix than "gpt-" for "gpt-4o-mini"
        assert_eq!(found.unwrap()[0].provider.name(), "specific");
    }

    #[test]
    fn provider_id_lookup() {
        let mut router = ModelRouter::new();
        let openai_id = Uuid::new_v4();
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "openai".into(),
        });
        router.register_route(
            "gpt-4o",
            RouteEntry {
                provider,
                provider_id: openai_id,
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );
        assert_eq!(router.provider_id_for("gpt-4o"), Some(openai_id));
        assert_eq!(router.provider_id_for("unknown"), None);
    }

    #[test]
    fn multiple_routes_sorted() {
        let mut router = ModelRouter::new();
        let primary: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "primary".into(),
        });
        let fallback: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: "fallback".into(),
        });
        router.register_route(
            "gpt-4o",
            RouteEntry {
                provider: fallback,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 1,
            },
        );
        router.register_route(
            "gpt-4o",
            RouteEntry {
                provider: primary,
                provider_id: Uuid::nil(),
                upstream_model: None,
                weight: 100,
                priority: 0,
            },
        );
        router.sort_routes();

        let entries = router.route("gpt-4o").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].provider.name(), "primary");
        assert_eq!(entries[1].provider.name(), "fallback");
    }
}
