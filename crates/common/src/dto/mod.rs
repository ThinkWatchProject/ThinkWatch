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
/// What the body still carries:
///   - `signing_key`: hex-encoded HMAC key. The browser CANNOT
///     read the httpOnly cookie that holds this same value, but
///     the page JS still needs the key to compute write-request
///     signatures, so we hand it back in the body once and the
///     frontend stashes it in `sessionStorage`. (sessionStorage,
///     not localStorage, so it dies with the tab.)
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
    /// Per-session HMAC signing key (hex-encoded, 32 bytes).
    /// Used by the frontend to sign state-changing requests.
    pub signing_key: String,
    /// Flat union of every role's `permissions` field — the
    /// authoritative set the UI uses for hasPermission() checks.
    #[serde(default)]
    pub permissions: Vec<String>,
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

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub is_active: bool,
    /// All role assignments for this user (system + custom, union).
    #[serde(default)]
    pub role_assignments: Vec<RoleAssignment>,
    /// Flat union of every role's `permissions` field. Returned
    /// from `/api/auth/me` so the frontend hasPermission() helper
    /// can populate without needing to decode the access token
    /// (which is now an httpOnly cookie unreadable from JS).
    #[serde(default)]
    pub permissions: Vec<String>,
    pub created_at: DateTime<Utc>,
}

// --- API Key DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub team_id: Option<Uuid>,
    /// Which gateways this key can call. Must be non-empty;
    /// each entry must be `ai_gateway` or `mcp_gateway`. The
    /// handler validates and normalizes the list.
    pub surfaces: Vec<String>,
    pub allowed_models: Option<Vec<String>>,
    pub expires_in_days: Option<i32>,
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
    pub api_key: String,
    pub config: Option<serde_json::Value>,
    /// Custom HTTP headers forwarded when proxying requests to this provider.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

// --- MCP Server DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    pub description: Option<String>,
    pub endpoint_url: String,
    pub transport_type: Option<String>,
    pub auth_type: Option<String>,
    pub auth_secret: Option<String>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
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
