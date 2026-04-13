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
    pub auth_type: Option<String>,
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
