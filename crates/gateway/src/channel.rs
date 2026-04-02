use crate::providers::DynAiProvider;
use rand::RngExt;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A channel is a named provider endpoint with priority and weight.
pub struct Channel {
    pub id: String,
    pub name: String,
    pub provider: Arc<dyn DynAiProvider>,
    pub priority: i32,
    pub weight: u32,
    pub enabled: bool,
    pub models: Vec<String>,
}

/// Read-only view of a channel for admin API responses.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub provider_name: String,
    pub priority: i32,
    pub weight: u32,
    pub enabled: bool,
    pub models: Vec<String>,
}

/// Selects channels based on priority groups and weighted random within groups.
pub struct ChannelScheduler {
    channels: RwLock<Vec<Channel>>,
}

impl Default for ChannelScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelScheduler {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(Vec::new()),
        }
    }

    /// Add a channel to the scheduler.
    pub async fn add_channel(&self, channel: Channel) {
        self.channels.write().await.push(channel);
    }

    /// Remove a channel by ID.
    pub async fn remove_channel(&self, id: &str) {
        self.channels.write().await.retain(|c| c.id != id);
    }

    /// Enable a channel by ID.
    pub async fn enable_channel(&self, id: &str) {
        let mut channels = self.channels.write().await;
        if let Some(ch) = channels.iter_mut().find(|c| c.id == id) {
            ch.enabled = true;
        }
    }

    /// Disable a channel by ID.
    pub async fn disable_channel(&self, id: &str) {
        let mut channels = self.channels.write().await;
        if let Some(ch) = channels.iter_mut().find(|c| c.id == id) {
            ch.enabled = false;
        }
    }

    /// List all channels as read-only info structs.
    pub async fn list_channels(&self) -> Vec<ChannelInfo> {
        self.channels
            .read()
            .await
            .iter()
            .map(|c| ChannelInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                provider_name: c.provider.name().to_string(),
                priority: c.priority,
                weight: c.weight,
                enabled: c.enabled,
                models: c.models.clone(),
            })
            .collect()
    }

    /// Select a provider for the given model using priority + weighted random.
    ///
    /// 1. Filter channels that support this model AND are enabled
    /// 2. Group by priority (lowest number = highest priority)
    /// 3. Within the highest priority group, select by weighted random
    pub async fn select(&self, model: &str) -> Option<Arc<dyn DynAiProvider>> {
        let channels = self.channels.read().await;

        // Filter to enabled channels that support this model
        let mut candidates: Vec<&Channel> = channels
            .iter()
            .filter(|c| c.enabled && c.models.iter().any(|m| m == model))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Sort by priority (ascending — lower number = higher priority)
        candidates.sort_by_key(|c| c.priority);

        // Find the highest priority (lowest number)
        let top_priority = candidates[0].priority;

        // Get all channels in the top priority group
        let top_group: Vec<&Channel> = candidates
            .into_iter()
            .take_while(|c| c.priority == top_priority)
            .collect();

        Some(weighted_select(&top_group))
    }

    /// Select a provider with fallback through priority groups.
    ///
    /// If the selected channel from the highest priority group fails, try
    /// the next channel in the same group, then fall through to lower priority groups.
    pub async fn select_with_fallback(
        &self,
        model: &str,
    ) -> Vec<Arc<dyn DynAiProvider>> {
        let channels = self.channels.read().await;

        let mut candidates: Vec<&Channel> = channels
            .iter()
            .filter(|c| c.enabled && c.models.iter().any(|m| m == model))
            .collect();

        if candidates.is_empty() {
            return Vec::new();
        }

        candidates.sort_by_key(|c| c.priority);

        // Return providers ordered: weighted-random within each priority group,
        // groups ordered by priority
        let mut result = Vec::new();
        let mut i = 0;
        while i < candidates.len() {
            let priority = candidates[i].priority;
            let group_end = candidates[i..]
                .iter()
                .position(|c| c.priority != priority)
                .map(|pos| i + pos)
                .unwrap_or(candidates.len());

            let group = &candidates[i..group_end];
            // Shuffle group by weighted random
            let mut group_vec: Vec<&Channel> = group.to_vec();
            weighted_shuffle(&mut group_vec);
            for ch in group_vec {
                result.push(Arc::clone(&ch.provider));
            }

            i = group_end;
        }

        result
    }
}

/// Select one channel from a group using weighted random.
fn weighted_select(group: &[&Channel]) -> Arc<dyn DynAiProvider> {
    if group.len() == 1 {
        return Arc::clone(&group[0].provider);
    }

    let total_weight: u32 = group.iter().map(|c| c.weight).sum();
    if total_weight == 0 {
        return Arc::clone(&group[0].provider);
    }

    let mut rng = rand::rng();
    let pick = rng.random_range(0..total_weight);

    let mut cumulative = 0u32;
    for ch in group {
        cumulative += ch.weight;
        if pick < cumulative {
            return Arc::clone(&ch.provider);
        }
    }

    Arc::clone(&group.last().unwrap().provider)
}

/// Shuffle channels in-place using weighted probability.
fn weighted_shuffle(group: &mut Vec<&Channel>) {
    let len = group.len();
    if len <= 1 {
        return;
    }

    let mut rng = rand::rng();
    for i in 0..len - 1 {
        let remaining = &group[i..];
        let total_weight: u32 = remaining.iter().map(|c| c.weight).sum();
        if total_weight == 0 {
            break;
        }
        let pick = rng.random_range(0..total_weight);
        let mut cumulative = 0u32;
        let mut selected = 0;
        for (j, ch) in remaining.iter().enumerate() {
            cumulative += ch.weight;
            if pick < cumulative {
                selected = j;
                break;
            }
        }
        group.swap(i, i + selected);
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

    fn make_channel(
        id: &str,
        name: &str,
        priority: i32,
        weight: u32,
        models: Vec<&str>,
    ) -> Channel {
        Channel {
            id: id.to_string(),
            name: name.to_string(),
            provider: Arc::new(DummyProvider {
                provider_name: name.to_string(),
            }),
            priority,
            weight,
            enabled: true,
            models: models.into_iter().map(String::from).collect(),
        }
    }

    #[tokio::test]
    async fn select_returns_none_for_unknown_model() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "openai", 0, 100, vec!["gpt-4o"]))
            .await;

        assert!(scheduler.select("unknown-model").await.is_none());
    }

    #[tokio::test]
    async fn select_returns_provider_for_matching_model() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "openai", 0, 100, vec!["gpt-4o"]))
            .await;

        let provider = scheduler.select("gpt-4o").await;
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().name(), "openai");
    }

    #[tokio::test]
    async fn higher_priority_preferred() {
        let scheduler = ChannelScheduler::new();
        // Priority 0 (highest)
        scheduler
            .add_channel(make_channel("1", "primary", 0, 100, vec!["gpt-4o"]))
            .await;
        // Priority 1 (lower)
        scheduler
            .add_channel(make_channel("2", "secondary", 1, 100, vec!["gpt-4o"]))
            .await;

        // Run multiple times — should always pick priority 0
        for _ in 0..20 {
            let provider = scheduler.select("gpt-4o").await.unwrap();
            assert_eq!(provider.name(), "primary");
        }
    }

    #[tokio::test]
    async fn weighted_selection_within_priority_group() {
        let scheduler = ChannelScheduler::new();
        // Both priority 0, but different weights
        scheduler
            .add_channel(make_channel("1", "heavy", 0, 900, vec!["gpt-4o"]))
            .await;
        scheduler
            .add_channel(make_channel("2", "light", 0, 100, vec!["gpt-4o"]))
            .await;

        let mut heavy_count = 0;
        let mut light_count = 0;
        for _ in 0..1000 {
            let provider = scheduler.select("gpt-4o").await.unwrap();
            match provider.name() {
                "heavy" => heavy_count += 1,
                "light" => light_count += 1,
                _ => panic!("unexpected provider"),
            }
        }

        // Heavy should get roughly 90% of selections
        assert!(
            heavy_count > 800,
            "heavy should dominate: heavy={heavy_count}, light={light_count}"
        );
        assert!(light_count > 0, "light should get some selections");
    }

    #[tokio::test]
    async fn disabled_channels_excluded() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "enabled", 0, 100, vec!["gpt-4o"]))
            .await;
        scheduler
            .add_channel(make_channel("2", "disabled", 0, 100, vec!["gpt-4o"]))
            .await;
        scheduler.disable_channel("2").await;

        for _ in 0..20 {
            let provider = scheduler.select("gpt-4o").await.unwrap();
            assert_eq!(provider.name(), "enabled");
        }
    }

    #[tokio::test]
    async fn enable_disable_toggle() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "provider", 0, 100, vec!["gpt-4o"]))
            .await;

        scheduler.disable_channel("1").await;
        assert!(scheduler.select("gpt-4o").await.is_none());

        scheduler.enable_channel("1").await;
        assert!(scheduler.select("gpt-4o").await.is_some());
    }

    #[tokio::test]
    async fn remove_channel_works() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "provider", 0, 100, vec!["gpt-4o"]))
            .await;

        scheduler.remove_channel("1").await;
        assert!(scheduler.select("gpt-4o").await.is_none());
    }

    #[tokio::test]
    async fn list_channels_returns_all() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "openai", 0, 100, vec!["gpt-4o"]))
            .await;
        scheduler
            .add_channel(make_channel("2", "anthropic", 1, 50, vec!["claude-3"]))
            .await;

        let list = scheduler.list_channels().await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "1");
        assert_eq!(list[1].id, "2");
    }

    #[tokio::test]
    async fn select_with_fallback_orders_by_priority() {
        let scheduler = ChannelScheduler::new();
        scheduler
            .add_channel(make_channel("1", "primary", 0, 100, vec!["gpt-4o"]))
            .await;
        scheduler
            .add_channel(make_channel("2", "secondary", 1, 100, vec!["gpt-4o"]))
            .await;
        scheduler
            .add_channel(make_channel("3", "tertiary", 2, 100, vec!["gpt-4o"]))
            .await;

        let providers = scheduler.select_with_fallback("gpt-4o").await;
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].name(), "primary");
        assert_eq!(providers[1].name(), "secondary");
        assert_eq!(providers[2].name(), "tertiary");
    }
}
