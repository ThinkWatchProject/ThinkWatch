use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    // Budget caps live in `budget_caps` (subject_kind = 'team').
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TeamMember {
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub role: String,
    pub joined_at: DateTime<Utc>,
}
