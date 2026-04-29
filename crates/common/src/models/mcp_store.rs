use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct McpStoreTemplate {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub endpoint_template: Option<String>,
    /// OAuth client config that will be copied onto the new
    /// `mcp_servers` row when this template is installed. The admin can
    /// still edit these (or paste a `client_id`/`client_secret`) before
    /// committing.
    pub oauth_issuer: Option<String>,
    pub oauth_authorization_endpoint: Option<String>,
    pub oauth_token_endpoint: Option<String>,
    pub oauth_revocation_endpoint: Option<String>,
    /// Userinfo endpoint copied onto the new server row at install
    /// time. See [`crate::models::McpServer::oauth_userinfo_endpoint`].
    pub oauth_userinfo_endpoint: Option<String>,
    pub oauth_default_scopes: Vec<String>,
    /// `true` ⇒ users can paste their own PAT / API key. Mirrored onto
    /// `mcp_servers.allow_static_token` at install time.
    pub allow_static_token: bool,
    pub static_token_help_url: Option<String>,
    pub auth_instructions: Option<String>,
    pub deploy_type: Option<String>,
    pub deploy_command: Option<String>,
    pub deploy_docs_url: Option<String>,
    pub homepage_url: Option<String>,
    pub repo_url: Option<String>,
    pub featured: bool,
    pub install_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
