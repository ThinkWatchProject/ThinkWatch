//! # DTO placement convention
//!
//! A type belongs here (in `think_watch_common::dto`) when **more than
//! one crate** names it — auth/gateway/mcp-gateway/server all referring
//! to the same request or response shape. Classic examples:
//! `LoginRequest`, `PaginationParams`, `CreateApiKeyResponse`.
//!
//! A type belongs **next to its handler** (private or `pub(crate)` in
//! the handler module) when only that one handler parses or emits it —
//! `TestMcpServerRequest`, `ForceRevokeRequest`, `UpdateKeyRequest`.
//! Moving those here would bloat the crate without adding a seam.
//!
//! When a handler-local type grows a second caller (another handler,
//! a gateway, the CLI, etc.), that's the promotion trigger — lift it
//! into this module and update the imports in one pass. Until then,
//! leave it where the request actually lands. The rule is "shared
//! types converge here"; it isn't "every DTO lives here".

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// --- Auth DTOs ---

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    /// TOTP code for two-factor authentication (required if user has TOTP enabled).
    pub totp_code: Option<String>,
}

/// Login / refresh / SSO callback response.
///
/// **Tokens are NOT in the body anymore.** access_token and
/// refresh_token are delivered exclusively as httpOnly cookies
/// (`Set-Cookie: access_token=...; HttpOnly; Secure; SameSite=Lax`)
/// so client-side JavaScript — and any XSS attacker who lands in
/// that JS — cannot read them. The frontend never sees the JWT
/// payload directly.
///
/// **No signing key in the body.** Request signing uses ECDSA P-256
/// with client-generated key pairs. The client generates a key pair
/// after login and registers the public key via POST /api/auth/register-key.
/// The private key (non-extractable) lives in IndexedDB.
///
/// What the body still carries:
///   - `permissions` / `roles`: the JWT claims the frontend used
///     to decode out of the access token to gate UI buttons. With
///     the token now opaque from JS, the server has to surface
///     them explicitly.
///   - `expires_in`: how long the access cookie is valid, so the
///     frontend can schedule a proactive refresh before it expires.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token_type: String,
    pub expires_in: i64,
    /// Flat union of every role's `permissions` field — the
    /// authoritative set the UI uses for hasPermission() checks.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Permissions explicitly denied by policy documents.
    #[serde(default)]
    pub denied_permissions: Vec<String>,
    /// Role names (system + custom union). Cosmetic — used by the
    /// UI for badges, never for authorization decisions.
    #[serde(default)]
    pub roles: Vec<String>,
    /// If true, the user must change their password before using the platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_change_required: Option<bool>,
}

/// Returned when login credentials are valid but TOTP verification is needed.
#[derive(Debug, Serialize)]
pub struct TotpRequiredResponse {
    pub totp_required: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct RefreshRequest {
    /// Optional. When omitted, the server reads `refresh_token`
    /// from the httpOnly cookie set at login time. The body field
    /// is kept for any non-browser clients that might still post
    /// the token explicitly, but the cookie path is the standard
    /// browser flow.
    #[serde(default)]
    pub refresh_token: Option<String>,
}

// --- User DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

/// One role assignment for a user. Unified over system and custom
/// roles — the frontend uses `is_system` to style the badge and to
/// decide whether the role can be removed/edited.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RoleAssignment {
    pub role_id: Uuid,
    pub name: String,
    pub is_system: bool,
    pub scope: String,
}

/// Request shape for creating / updating a user's role assignments.
/// `scope` defaults to `"global"` when omitted.
#[derive(Debug, Deserialize, Clone)]
pub struct RoleAssignmentRequest {
    pub role_id: Uuid,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Lightweight team summary embedded in `UserResponse` so the
/// admin user list can display team memberships without a second
/// round-trip per row.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserTeamSummary {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub is_active: bool,
    /// OIDC subject identifier, present iff the user was provisioned
    /// (or last logged in) via SSO. Surfaced to the admin user list
    /// so the OIDC badge column has a real source — without this the
    /// frontend was reading an undefined field and never showing it.
    #[serde(default)]
    pub oidc_subject: Option<String>,
    /// All role assignments for this user (system + custom, union).
    #[serde(default)]
    pub role_assignments: Vec<RoleAssignment>,
    /// Flat union of every role's `permissions` field. Returned
    /// from `/api/auth/me` so the frontend hasPermission() helper
    /// can populate without needing to decode the access token
    /// (which is now an httpOnly cookie unreadable from JS).
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Permissions explicitly denied by policy documents.
    #[serde(default)]
    pub denied_permissions: Vec<String>,
    /// Teams this user is a member of. Used by the admin user
    /// list to render scope context: a team_manager looking at
    /// their merged-team list can tell which row belongs to
    /// which team.
    #[serde(default)]
    pub teams: Vec<UserTeamSummary>,
    pub created_at: DateTime<Utc>,
}

// --- API Key DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    /// Which gateways this key can call. Must be non-empty;
    /// each entry must be `ai_gateway` or `mcp_gateway`. The
    /// handler validates and normalizes the list.
    pub surfaces: Vec<String>,
    pub allowed_models: Option<Vec<String>>,
    /// MCP tool allow-list, parallel to `allowed_models`. `None`
    /// = unrestricted; must be a subset of the caller's
    /// role-granted `allowed_mcp_tools` (enforced by the handler).
    pub allowed_mcp_tools: Option<Vec<String>>,
    pub expires_in_days: Option<i32>,
    /// Optional cost-center / project tag for analytics attribution.
    /// Free-form up to 64 chars; the admin UI autocompletes from the
    /// distinct values already in use.
    pub cost_center: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateApiKeyResponse {
    pub id: Uuid,
    pub key: String, // plaintext, shown only once
    pub name: String,
    pub key_prefix: String,
}

// --- Provider DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateProviderRequest {
    pub name: String,
    pub display_name: String,
    pub provider_type: String,
    pub base_url: String,
    /// Unified request headers (auth + custom + identity templates).
    /// Stored in config_json.headers as `[{key, value}]`.
    #[serde(default)]
    pub headers: Vec<ProviderHeader>,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone, utoipa::ToSchema)]
pub struct ProviderHeader {
    pub key: String,
    pub value: String,
}

// --- MCP Server DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    /// Optional short identifier used as tool namespace prefix. If omitted,
    /// derived automatically from `name`. Must match `[a-z0-9_]{1,32}`.
    pub namespace_prefix: Option<String>,
    pub description: Option<String>,
    pub endpoint_url: String,
    pub transport_type: Option<String>,
    pub auth_type: Option<String>,
    pub auth_secret: Option<String>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    /// Values may contain `{{user_id}}` and `{{user_email}}` template
    /// variables which are resolved per-request from the caller's identity.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    /// Per-server response cache TTL in seconds. `None` = use global
    /// `mcp.cache_ttl_secs` setting. `0` = disable caching for this server.
    pub cache_ttl_secs: Option<u64>,
}

// --- Pagination ---

#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

impl PaginationParams {
    pub fn offset(&self) -> u32 {
        let page = self.page.unwrap_or(1).max(1);
        let per_page = self.per_page();
        (page - 1) * per_page
    }

    pub fn per_page(&self) -> u32 {
        self.per_page.unwrap_or(20).min(100)
    }
}

#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T: Serialize> {
    pub data: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub per_page: u32,
}
