use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UsageRecord {
    pub id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub provider_id: Option<Uuid>,
    /// Snapshot of providers.name at insert time. Survives provider
    /// deletion (provider_id becomes NULL) so cost attribution stays
    /// intact across provider churn.
    pub provider_name: Option<String>,
    pub model_id: String,
    pub request_type: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub cost_usd: Decimal,
    pub latency_ms: Option<i32>,
    pub status_code: Option<i32>,
    pub created_at: DateTime<Utc>,
}
