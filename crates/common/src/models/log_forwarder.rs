use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LogForwarder {
    pub id: Uuid,
    pub name: String,
    pub forwarder_type: String,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub sent_count: i64,
    pub error_count: i64,
    pub last_sent_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub log_types: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
