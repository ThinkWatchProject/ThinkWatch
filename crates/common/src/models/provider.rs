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
    #[serde(skip_serializing)]
    pub api_key_encrypted: Vec<u8>,
    pub is_active: bool,
    pub config_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Model {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub model_id: String,
    pub display_name: String,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    /// Quota multiplier on input tokens. The limits engine multiplies
    /// raw token counts by this to get "weighted tokens" so a budget
    /// cap can survive a single gpt-4o burst. Defaults to 1.0 in the
    /// schema; tune via the admin Models page.
    pub input_multiplier: Decimal,
    pub output_multiplier: Decimal,
    pub is_active: bool,
}
