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
    /// Optional data-residency tag (e.g. `us-east-1`, `eu-west-1`).
    /// Snapshotted into `gateway_logs.region` on every request so
    /// residency-aware analytics can GROUP BY region without joining
    /// back to this row (which may be soft-deleted).
    pub region: Option<String>,
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
}
