use crate::providers::DynAiProvider;
use crate::strategy::RoutingStrategy;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// A single route entry mapping a model to a provider.
///
/// `weight` is both the load-balancing knob and the A/B traffic-split
/// control under the `weighted` strategy: two routes with weights
/// `(50, 50)` give a 1:1 split, `(90, 10)` is a 90/10 canary, `(0, 100)`
/// shadows traffic off without removing the row. Under the auto
/// strategies (`latency`, `cost`, `latency_cost`) the operator weight
/// becomes a multiplicative bias on the strategy-derived score.
///
/// All routes for a model are peers — there is no priority tier.
/// Failover happens implicitly: the proxy filters out unhealthy
/// (circuit-open) and capped-out routes before applying the strategy,
/// so a degraded upstream stops getting traffic without admin
/// intervention. To run an A/B between two upstream model names on the
/// same provider, register two routes with different `upstream_model`
/// values and the desired weights.
pub struct RouteEntry {
    pub provider: Arc<dyn DynAiProvider>,
    pub provider_id: Uuid,
    /// `model_routes.id` — stable identifier used by the health
    /// tracker (Redis keys), the decision log, and route-mode
    /// affinity. Each `(model_id, provider_id, upstream_model)`
    /// triple has exactly one route_id per the unique constraint.
    pub route_id: Uuid,
    /// Snapshot of `providers.name` at route-load time. We carry this
    /// alongside `provider_id` so the post-request `gateway_logs.provider`
    /// column gets a human-readable string without a second DB hit per
    /// request, and the value survives provider deletion (where the
    /// foreign key flips to NULL but the log row stays legible).
    pub provider_name: String,
    /// Upstream model name to send to the provider. `Some(name)` for
    /// every DB-backed route (the column is NOT NULL). `None` only
    /// occurs on the synthetic prefix-fallback routes the bootstrap
    /// installs for providers with zero configured routes — there the
    /// runtime forwards whatever model the client requested.
    pub upstream_model: Option<String>,
    pub weight: u32,
    /// Optional human-readable identifier. Surfaced in the admin UI
    /// for routes whose `provider_name + upstream_model` alone aren't
    /// distinctive enough (e.g. "EU-primary", "GPU-cluster-A"). Pure
    /// metadata — the runtime ignores it.
    pub label: Option<String>,
    /// Per-route RPM cap (NULL in DB ⇒ None ⇒ unlimited).
    pub rpm_cap: Option<u32>,
    /// Per-route TPM cap.
    pub tpm_cap: Option<u32>,
}

/// Per-model overrides for routing strategy / affinity. `None` on
/// any field ⇒ fall through to the gateway-wide default
/// (`gateway.default_*` in system_settings).
#[derive(Debug, Clone, Default)]
pub struct ModelRoutingConfig {
    pub strategy: Option<RoutingStrategy>,
    pub affinity_mode: Option<AffinityMode>,
    pub affinity_ttl_secs: Option<u32>,
}

/// Affinity scope — see `proxy.rs` for the runtime semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AffinityMode {
    /// Stateless. Strategy decides every request.
    None,
    /// Stick to the same provider — preserves prompt-cache hit rate
    /// when one provider serves multiple upstream models.
    #[default]
    Provider,
    /// Stick to the same `route_id` — strict A/B adherence within a
    /// session even when the same provider has multiple routes.
    Route,
}

impl AffinityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Provider => "provider",
            Self::Route => "route",
        }
    }

    pub fn parse_or_default(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "route" => Self::Route,
            _ => Self::Provider,
        }
    }
}

/// Routes model names to AI provider implementations with strategy-
/// driven traffic splitting and circuit-breaker-driven failover.
///
/// All routes for a model are peers; there is no priority tier.
/// Selection: the proxy prunes the candidate set down to enabled,
/// circuit-closed, under-cap routes, then applies the model's
/// strategy (defaulting to `gateway.default_routing_strategy`) to pick
/// one. On retryable error the proxy advances to another candidate
/// from the same set.
///
/// Also supports prefix-match as a fallback (e.g. `"gpt-" -> OpenAiProvider`)
/// for providers that have no explicit model routes configured.
pub struct ModelRouter {
    /// Exact model name -> list of routes, sorted by weight DESC for
    /// deterministic candidate ordering (helps test stability and
    /// makes the decision log easier to scan).
    routes: HashMap<String, Vec<RouteEntry>>,
    /// Per-model routing overrides. Lookup falls through to global
    /// defaults (`gateway.default_*` in system_settings) when absent.
    configs: HashMap<String, ModelRoutingConfig>,
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
            configs: HashMap::new(),
        }
    }

    /// Set per-model routing config (strategy / affinity overrides).
    /// Called once per model at router-load time, after all routes
    /// have been registered.
    pub fn set_model_config(&mut self, model_id: &str, cfg: ModelRoutingConfig) {
        self.configs.insert(model_id.to_string(), cfg);
    }

    /// Look up the routing config for a model. Returns the empty
    /// (all-`None`) config when absent — the caller fills in global
    /// defaults from DynamicConfig at request time. Done this way so
    /// the router doesn't have to read DynamicConfig itself, keeping
    /// it dependency-free for unit tests.
    pub fn config_for(&self, model_id: &str) -> ModelRoutingConfig {
        if let Some(c) = self.configs.get(model_id) {
            return c.clone();
        }
        // Symmetric with `route()`'s prefix fallback.
        let mut best: Option<(&str, &ModelRoutingConfig)> = None;
        for (pattern, cfg) in &self.configs {
            if model_id.starts_with(pattern.as_str()) {
                match best {
                    Some((cur, _)) if pattern.len() > cur.len() => best = Some((pattern, cfg)),
                    None => best = Some((pattern, cfg)),
                    _ => {}
                }
            }
        }
        best.map(|(_, c)| c.clone()).unwrap_or_default()
    }

    /// Register a route for a given model.
    pub fn register_route(&mut self, model_id: &str, entry: RouteEntry) {
        self.routes
            .entry(model_id.to_string())
            .or_default()
            .push(entry);
    }

    /// Sort all route vecs by weight DESC. Call once after all routes
    /// have been registered. Order doesn't affect correctness (every
    /// strategy weighs candidates explicitly), just determinism in
    /// tests and decision-log readability.
    pub fn sort_routes(&mut self) {
        for entries in self.routes.values_mut() {
            entries.sort_by_key(|e| std::cmp::Reverse(e.weight));
        }
    }

    /// Look up all routes for a model (exact match then prefix fallback).
    /// Returns routes sorted by weight DESC.
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

    /// Test helper — collapse RouteEntry construction down to fields
    /// the tests actually assert on. New fields default to neutral
    /// values so tests don't break each time the struct grows.
    fn entry(name: &str, provider_id: Uuid, weight: u32) -> RouteEntry {
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider {
            provider_name: name.into(),
        });
        RouteEntry {
            provider,
            provider_id,
            route_id: Uuid::new_v4(),
            provider_name: name.into(),
            upstream_model: None,
            weight,
            label: None,
            rpm_cap: None,
            tpm_cap: None,
        }
    }

    #[test]
    fn exact_match() {
        let mut router = ModelRouter::new();
        router.register_route("gpt-4o", entry("openai", Uuid::nil(), 100));
        let found = router.route("gpt-4o");
        assert!(found.is_some());
        assert_eq!(found.unwrap()[0].provider.name(), "openai");
    }

    #[test]
    fn prefix_match() {
        let mut router = ModelRouter::new();
        router.register_route("gpt-", entry("openai", Uuid::nil(), 100));
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
        router.register_route("gpt-", entry("generic", Uuid::nil(), 100));
        router.register_route("gpt-4o", entry("specific", Uuid::nil(), 100));
        let found = router.route("gpt-4o-mini");
        assert!(found.is_some());
        // "gpt-4o" is a longer prefix than "gpt-" for "gpt-4o-mini"
        assert_eq!(found.unwrap()[0].provider.name(), "specific");
    }

    #[test]
    fn provider_id_lookup() {
        let mut router = ModelRouter::new();
        let openai_id = Uuid::new_v4();
        router.register_route("gpt-4o", entry("openai", openai_id, 100));
        assert_eq!(router.provider_id_for("gpt-4o"), Some(openai_id));
        assert_eq!(router.provider_id_for("unknown"), None);
    }

    #[test]
    fn routes_sorted_by_weight_desc() {
        let mut router = ModelRouter::new();
        router.register_route("gpt-4o", entry("light", Uuid::nil(), 30));
        router.register_route("gpt-4o", entry("heavy", Uuid::nil(), 70));
        router.sort_routes();
        let entries = router.route("gpt-4o").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].provider.name(), "heavy");
        assert_eq!(entries[1].provider.name(), "light");
    }

    #[test]
    fn config_falls_back_to_prefix() {
        let mut router = ModelRouter::new();
        router.register_route("gpt-", entry("openai", Uuid::nil(), 0));
        router.set_model_config(
            "gpt-",
            ModelRoutingConfig {
                strategy: Some(RoutingStrategy::Latency),
                affinity_mode: Some(AffinityMode::None),
                affinity_ttl_secs: Some(60),
            },
        );
        let cfg = router.config_for("gpt-4o-mini");
        assert_eq!(cfg.strategy, Some(RoutingStrategy::Latency));
        assert_eq!(cfg.affinity_mode, Some(AffinityMode::None));
    }

    #[test]
    fn config_exact_beats_prefix() {
        let mut router = ModelRouter::new();
        router.set_model_config(
            "gpt-",
            ModelRoutingConfig {
                strategy: Some(RoutingStrategy::Weighted),
                ..Default::default()
            },
        );
        router.set_model_config(
            "gpt-4o-mini",
            ModelRoutingConfig {
                strategy: Some(RoutingStrategy::Latency),
                ..Default::default()
            },
        );
        assert_eq!(
            router.config_for("gpt-4o-mini").strategy,
            Some(RoutingStrategy::Latency)
        );
    }
}
