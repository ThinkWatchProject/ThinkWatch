use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct McpServer {
    pub id: Uuid,
    pub name: String,
    pub namespace_prefix: String,
    pub description: Option<String>,
    pub endpoint_url: String,
    pub transport_type: String,
    /// OAuth client config — set when the upstream supports OAuth and an
    /// admin has registered the gateway as a client at the upstream's AS.
    /// `oauth_issuer` populated alone is enough to drive RFC 8414
    /// discovery; the explicit endpoint columns short-circuit discovery
    /// for upstreams that don't advertise it.
    pub oauth_issuer: Option<String>,
    pub oauth_authorization_endpoint: Option<String>,
    pub oauth_token_endpoint: Option<String>,
    pub oauth_revocation_endpoint: Option<String>,
    pub oauth_client_id: Option<String>,
    /// Encrypted `oauth_client_secret` (AES-GCM via `crypto::encrypt`).
    /// Skipped from any serialized form so the secret never leaves the
    /// backend.
    #[serde(skip_serializing)]
    pub oauth_client_secret_encrypted: Option<Vec<u8>>,
    pub oauth_scopes: Vec<String>,
    /// Userinfo endpoint hit after the OAuth callback to populate
    /// `mcp_user_credentials.upstream_subject`. Best-effort: the
    /// resolver tries JWT-decode first (no network), falls back to
    /// `GET {oauth_userinfo_endpoint}` with the access_token, walks
    /// the JSON for the first non-empty subject-like field, and
    /// gives up silently if both paths fail.
    pub oauth_userinfo_endpoint: Option<String>,
    /// `true` ⇒ users may paste their own static token (PAT / API key)
    /// in the per-user connections UI as an alternative (or sole) way
    /// to authenticate.
    pub allow_static_token: bool,
    /// Optional URL surfaced next to the "paste token" UI so users
    /// know where to generate the token.
    pub static_token_help_url: Option<String>,
    /// Snapshot of the upstream's `tools/list` from the most recent
    /// admin probe. Returned to users that haven't authorized yet so
    /// the catalog isn't silently empty; per-user calls bypass this.
    pub cached_tools_jsonb: Option<serde_json::Value>,
    pub cached_tools_at: Option<DateTime<Utc>>,
    pub status: String,
    pub health_check_interval: Option<i32>,
    pub last_health_check: Option<DateTime<Utc>>,
    /// Most recent failure message from tool discovery / health check.
    /// `NULL` means the last attempt succeeded. Surfaced in the admin UI
    /// so operators don't have to dig through server logs.
    #[serde(default)]
    pub last_error: Option<String>,
    pub config_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// Computed field: number of active tools discovered for this server.
    #[sqlx(default)]
    #[serde(default)]
    pub tools_count: i64,
    /// Computed field: lifetime call count (from ClickHouse mcp_logs).
    /// 0 when ClickHouse is unavailable.
    #[sqlx(default)]
    #[serde(default)]
    pub call_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct McpTool {
    pub id: Uuid,
    pub server_id: Uuid,
    pub tool_name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
    pub is_active: bool,
    pub discovered_at: DateTime<Utc>,
}
