use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Provider {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub provider_type: String,
    pub base_url: String,
    pub is_active: bool,
    pub config_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Model {
    pub id: Uuid,
    pub model_id: String,
    pub display_name: String,
    /// Relative input-token cost factor. Absolute `$/token` is
    /// `platform_pricing.input_price_per_token × input_weight`.
    pub input_weight: Decimal,
    /// Relative output-token cost factor.
    pub output_weight: Decimal,
    /// Per-model routing strategy override.
    /// `None` ⇒ inherit `gateway.default_routing_strategy`.
    /// One of `weighted` / `latency` / `health` / `latency_health`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_strategy: Option<String>,
    /// Per-model affinity mode override.
    /// `None` ⇒ inherit `gateway.default_affinity_mode`.
    /// One of `none` / `provider` / `route`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_mode: Option<String>,
    /// Per-model affinity TTL override (seconds, 0–86400).
    /// `None` ⇒ inherit `gateway.default_affinity_ttl_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_ttl_secs: Option<i32>,
    /// Free-form admin tags. Surfaced as chip badges in the UI;
    /// ignored by the routing layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}
