use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Represents a single client-facing MCP session that may fan out to multiple
/// upstream server sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSession {
    /// Unique session identifier (opaque string, not necessarily a UUID).
    pub id: String,
    /// The authenticated user who owns this session.
    pub user_id: Uuid,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// Last time any activity occurred on this session.
    pub last_active: DateTime<Utc>,
    /// Maps upstream server ID → the `Mcp-Session-Id` we received from that
    /// server, so we can resume upstream sessions across requests.
    pub upstream_sessions: HashMap<Uuid, String>,
}

/// Manages the lifecycle of client-facing MCP sessions.
///
/// Uses Redis as the backing store for multi-instance deployments.
/// Falls back to in-memory storage if Redis is not provided.
#[derive(Clone)]
pub struct SessionManager {
    /// In-memory fallback (used when Redis is not available, or for local caching)
    local: Arc<RwLock<HashMap<String, McpSession>>>,
    /// Optional Redis client for persistent session storage
    redis: Option<fred::clients::Client>,
    /// Session TTL in seconds
    ttl_secs: i64,
}

const SESSION_PREFIX: &str = "mcp_session:";

impl SessionManager {
    pub fn new() -> Self {
        Self {
            local: Arc::new(RwLock::new(HashMap::new())),
            redis: None,
            ttl_secs: 3600, // 1 hour default
        }
    }

    /// Create a SessionManager backed by Redis for multi-instance persistence.
    pub fn with_redis(redis: fred::clients::Client) -> Self {
        Self {
            local: Arc::new(RwLock::new(HashMap::new())),
            redis: Some(redis),
            ttl_secs: 3600,
        }
    }

    fn redis_key(id: &str) -> String {
        format!("{SESSION_PREFIX}{id}")
    }

    /// Create a new session for the given user, returning the session ID.
    pub async fn create_session(&self, user_id: Uuid) -> String {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let session = McpSession {
            id: id.clone(),
            user_id,
            created_at: now,
            last_active: now,
            upstream_sessions: HashMap::new(),
        };

        if let Some(ref redis) = self.redis
            && let Ok(json) = serde_json::to_string(&session)
        {
            let _: Result<(), _> = fred::interfaces::KeysInterface::set(
                redis,
                Self::redis_key(&id),
                json,
                Some(fred::types::Expiration::EX(self.ttl_secs)),
                None,
                false,
            )
            .await;
        }

        // Also store locally for fast lookups
        let mut sessions = self.local.write().await;
        sessions.insert(id.clone(), session);
        id
    }

    /// Retrieve a session by its ID.
    pub async fn get_session(&self, id: &str) -> Option<McpSession> {
        // Check local cache first
        {
            let sessions = self.local.read().await;
            if let Some(s) = sessions.get(id) {
                return Some(s.clone());
            }
        }

        // Fall back to Redis
        if let Some(ref redis) = self.redis {
            let json: Option<String> =
                fred::interfaces::KeysInterface::get(redis, Self::redis_key(id))
                    .await
                    .ok()
                    .flatten();
            if let Some(json) = json
                && let Ok(session) = serde_json::from_str::<McpSession>(&json)
            {
                // Populate local cache
                let mut local = self.local.write().await;
                local.insert(id.to_string(), session.clone());
                return Some(session);
            }
        }

        None
    }

    /// Touch the `last_active` timestamp on a session.
    pub async fn update_activity(&self, id: &str) {
        let mut sessions = self.local.write().await;
        if let Some(session) = sessions.get_mut(id) {
            session.last_active = Utc::now();

            // Sync to Redis
            if let Some(ref redis) = self.redis
                && let Ok(json) = serde_json::to_string(session)
            {
                let _: Result<(), _> = fred::interfaces::KeysInterface::set(
                    redis,
                    Self::redis_key(id),
                    json,
                    Some(fred::types::Expiration::EX(self.ttl_secs)),
                    None,
                    false,
                )
                .await;
            }
        }
    }

    /// Record an upstream session ID obtained from an MCP server.
    pub async fn set_upstream_session(
        &self,
        session_id: &str,
        server_id: Uuid,
        upstream_session_id: String,
    ) {
        let mut sessions = self.local.write().await;
        if let Some(session) = sessions.get_mut(session_id) {
            session
                .upstream_sessions
                .insert(server_id, upstream_session_id);

            // Sync to Redis
            if let Some(ref redis) = self.redis
                && let Ok(json) = serde_json::to_string(session)
            {
                let _: Result<(), _> = fred::interfaces::KeysInterface::set(
                    redis,
                    Self::redis_key(session_id),
                    json,
                    Some(fred::types::Expiration::EX(self.ttl_secs)),
                    None,
                    false,
                )
                .await;
            }
        }
    }

    /// Get the upstream session ID for a given server within a client session.
    pub async fn get_upstream_session(&self, session_id: &str, server_id: Uuid) -> Option<String> {
        let session = self.get_session(session_id).await?;
        session.upstream_sessions.get(&server_id).cloned()
    }

    /// Remove a session (e.g. on `DELETE /mcp`).
    pub async fn remove_session(&self, id: &str) -> Option<McpSession> {
        // Remove from Redis
        if let Some(ref redis) = self.redis {
            let _: Result<i64, _> =
                fred::interfaces::KeysInterface::del(redis, Self::redis_key(id)).await;
        }

        let mut sessions = self.local.write().await;
        sessions.remove(id)
    }

    /// Evict sessions that have been inactive for longer than `max_age`.
    pub async fn cleanup_expired(&self, max_age: Duration) {
        let cutoff =
            Utc::now() - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::hours(1));
        let mut sessions = self.local.write().await;
        let expired: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.last_active <= cutoff)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired {
            sessions.remove(id);
            // Also remove from Redis
            if let Some(ref redis) = self.redis {
                let _: Result<i64, _> =
                    fred::interfaces::KeysInterface::del(redis, Self::redis_key(id)).await;
            }
        }
    }

    /// Spawn a background task that periodically cleans up expired sessions.
    pub fn start_cleanup_task(self, interval: Duration, max_age: Duration) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                self.cleanup_expired(max_age).await;
                tracing::debug!("session cleanup sweep completed");
            }
        });
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_get_session() {
        let mgr = SessionManager::new();
        let user_id = Uuid::new_v4();
        let session_id = mgr.create_session(user_id).await;

        let session = mgr.get_session(&session_id).await;
        assert!(session.is_some());
        let session = session.unwrap();
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.id, session_id);
    }

    #[tokio::test]
    async fn get_nonexistent_session_returns_none() {
        let mgr = SessionManager::new();
        assert!(mgr.get_session("does-not-exist").await.is_none());
    }

    #[tokio::test]
    async fn cleanup_expired_removes_old_sessions() {
        let mgr = SessionManager::new();
        let user_id = Uuid::new_v4();
        let session_id = mgr.create_session(user_id).await;

        // Session exists
        assert!(mgr.get_session(&session_id).await.is_some());

        // Cleanup with a zero-duration max_age should remove everything
        mgr.cleanup_expired(Duration::from_secs(0)).await;

        assert!(
            mgr.get_session(&session_id).await.is_none(),
            "expired session should be removed"
        );
    }

    #[tokio::test]
    async fn cleanup_keeps_fresh_sessions() {
        let mgr = SessionManager::new();
        let user_id = Uuid::new_v4();
        let session_id = mgr.create_session(user_id).await;

        // Cleanup with a generous max_age should keep the session
        mgr.cleanup_expired(Duration::from_secs(3600)).await;

        assert!(
            mgr.get_session(&session_id).await.is_some(),
            "fresh session should be kept"
        );
    }
}
