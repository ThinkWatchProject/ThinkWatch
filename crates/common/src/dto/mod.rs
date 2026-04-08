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

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    /// Per-session HMAC signing key (hex-encoded, 32 bytes).
    /// Used by the frontend to sign state-changing requests.
    pub signing_key: String,
    /// If true, the user must change their password before using the platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_change_required: Option<bool>,
}

/// Returned when login credentials are valid but TOTP verification is needed.
#[derive(Debug, Serialize)]
pub struct TotpRequiredResponse {
    pub totp_required: bool,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
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
    pub created_at: DateTime<Utc>,
}

// --- API Key DTOs ---

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub team_id: Option<Uuid>,
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
