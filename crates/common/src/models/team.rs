use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    // A team has no limits of its own: rate limits and budgets attach to
    // users and API keys (`rate_limit_rules` / `budget_caps` take
    // `subject_kind` 'user' or 'api_key_lineage'), and a team's members
    // get the limits of the roles the team grants them.
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TeamMember {
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub role: String,
    pub joined_at: DateTime<Utc>,
}
