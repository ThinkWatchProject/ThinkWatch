use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    pub key_prefix: String,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub name: String,
    pub user_id: Option<Uuid>,
    /// Which gateway surfaces this key may call. Always non-empty
    /// (DB CHECK enforces it). Each entry is one of `ai_gateway` /
    /// `mcp_gateway`.
    pub surfaces: Vec<String>,
    pub allowed_models: Option<Vec<String>>,
    // Rate limits and budget caps are stored in `rate_limit_rules` /
    // `budget_caps` (subject_kind = 'api_key').
    pub expires_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    // Lifecycle management fields
    pub deleted_at: Option<DateTime<Utc>>,
    pub rotation_period_days: Option<i32>,
    pub rotated_from_id: Option<Uuid>,
    pub grace_period_ends_at: Option<DateTime<Utc>>,
    pub inactivity_timeout_days: Option<i32>,
    pub disabled_reason: Option<String>,
    pub last_rotation_at: Option<DateTime<Utc>>,
    /// Optional cost-center / project tag for per-subject analytics
    /// group-by. Free-form up to 64 chars, or NULL when untagged.
    pub cost_center: Option<String>,
}
