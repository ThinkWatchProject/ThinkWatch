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
    /// MCP tool allow-list, parallel to `allowed_models` but for the
    /// MCP gateway. `None` = unrestricted; entries are namespaced tool
    /// keys (`github__list_issues`) or per-server wildcards
    /// (`github__*`). The middleware intersects this with the bearer's
    /// role-granted `allowed_mcp_tools` at request time.
    pub allowed_mcp_tools: Option<Vec<String>>,
    /// Per-server MCP account override map: `{ "<server_uuid>":
    /// "<account_label>" }`. When this key calls an MCP tool the
    /// gateway resolves the upstream credential by looking up the
    /// `(server_id, user_id, account_label)` row in
    /// `mcp_user_credentials` instead of falling through to the user's
    /// `is_default` credential. Empty `{}` ⇒ always default.
    #[serde(default)]
    pub mcp_account_overrides: serde_json::Value,
    // Rate limits and budget caps are stored in `rate_limit_rules` /
    // `budget_caps` (subject_kind = 'api_key_lineage', subject_id =
    // lineage_id) so they survive rotation across all generations.
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
    /// Stable identity that survives rotation. Brand-new keys
    /// self-reference (`lineage_id == id`); rotated keys inherit
    /// the parent's `lineage_id`. Every row in a rotation chain
    /// shares the same value, so per-key analytics roll up across
    /// generations on a single index lookup. Backed by
    /// `idx_api_keys_lineage_id`.
    ///
    /// Server-internal: skipped from JSON responses so the frontend
    /// stays unaware of rotation generations. Per-key history is
    /// rolled up by the read handlers via PG-side
    /// `id → lineage_id` resolution.
    #[serde(skip_serializing)]
    pub lineage_id: Uuid,
    /// Optional cost-center / project tag for per-subject analytics
    /// group-by. Free-form up to 64 chars, or NULL when untagged.
    pub cost_center: Option<String>,
}
