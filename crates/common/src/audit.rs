use chrono::Utc;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::models::LogForwarder;

// ---------------------------------------------------------------------------
// Log types — each maps to a distinct Quickwit index
// ---------------------------------------------------------------------------

/// The category of a log entry, determining which Quickwit index it's stored in
/// and which forwarders will receive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogType {
    /// API request audit (gateway usage)
    Audit,
    /// Gateway request logs (model calls, tokens, costs)
    Gateway,
    /// MCP tool invocation logs
    Mcp,
    /// Platform management operations (user/team/provider/settings changes)
    Platform,
}

impl LogType {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogType::Audit => "audit",
            LogType::Gateway => "gateway",
            LogType::Mcp => "mcp",
            LogType::Platform => "platform",
        }
    }

    pub fn index_id(&self) -> &'static str {
        match self {
            LogType::Audit => "audit_logs",
            LogType::Gateway => "gateway_logs",
            LogType::Mcp => "mcp_logs",
            LogType::Platform => "platform_logs",
        }
    }
}

/// Audit log entry sent to Quickwit and dynamically configured forwarders.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: String,
    #[serde(skip)]
    pub log_type: LogType,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub resource_id: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: String,
}

impl AuditEntry {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            log_type: LogType::Audit,
            user_id: None,
            api_key_id: None,
            action: action.into(),
            resource: None,
            resource_id: None,
            detail: None,
            ip_address: None,
            user_agent: None,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    /// Create entry for platform management operations.
    pub fn platform(action: impl Into<String>) -> Self {
        let mut entry = Self::new(action);
        entry.log_type = LogType::Platform;
        entry
    }

    /// Create entry for gateway request logs.
    pub fn gateway(action: impl Into<String>) -> Self {
        let mut entry = Self::new(action);
        entry.log_type = LogType::Gateway;
        entry
    }

    /// Create entry for MCP tool invocation logs.
    pub fn mcp(action: impl Into<String>) -> Self {
        let mut entry = Self::new(action);
        entry.log_type = LogType::Mcp;
        entry
    }

    pub fn log_type(mut self, lt: LogType) -> Self {
        self.log_type = lt;
        self
    }

    pub fn user_id(mut self, id: Uuid) -> Self {
        self.user_id = Some(id.to_string());
        self
    }

    pub fn api_key_id(mut self, id: Uuid) -> Self {
        self.api_key_id = Some(id.to_string());
        self
    }

    pub fn resource(mut self, r: impl Into<String>) -> Self {
        self.resource = Some(r.into());
        self
    }

    pub fn resource_id(mut self, r: impl Into<String>) -> Self {
        self.resource_id = Some(r.into());
        self
    }

    pub fn detail(mut self, d: serde_json::Value) -> Self {
        self.detail = Some(d);
        self
    }

    pub fn ip_address(mut self, ip: impl Into<String>) -> Self {
        self.ip_address = Some(ip.into());
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_builder_pattern() {
        let user_id = Uuid::new_v4();
        let api_key_id = Uuid::new_v4();
        let detail = serde_json::json!({"model": "claude-3"});

        let entry = AuditEntry::new("api.request")
            .user_id(user_id)
            .api_key_id(api_key_id)
            .resource("/v1/chat/completions")
            .detail(detail.clone())
            .ip_address("10.0.0.1")
            .user_agent("curl/8.0");

        assert_eq!(entry.action, "api.request");
        assert_eq!(entry.log_type, LogType::Audit);
        assert_eq!(
            entry.user_id.as_deref(),
            Some(user_id.to_string()).as_deref()
        );
        assert_eq!(
            entry.api_key_id.as_deref(),
            Some(api_key_id.to_string()).as_deref()
        );
        assert_eq!(entry.resource.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(entry.detail, Some(detail));
        assert_eq!(entry.ip_address.as_deref(), Some("10.0.0.1"));
        assert_eq!(entry.user_agent.as_deref(), Some("curl/8.0"));
        assert!(!entry.id.is_empty());
        assert!(!entry.created_at.is_empty());
    }

    #[test]
    fn platform_entry_has_correct_type() {
        let entry = AuditEntry::platform("user.created");
        assert_eq!(entry.log_type, LogType::Platform);
        assert_eq!(entry.action, "user.created");
    }

    #[test]
    fn gateway_entry_has_correct_type() {
        let entry = AuditEntry::gateway("chat.completion");
        assert_eq!(entry.log_type, LogType::Gateway);
    }

    #[test]
    fn mcp_entry_has_correct_type() {
        let entry = AuditEntry::mcp("tool.invoke");
        assert_eq!(entry.log_type, LogType::Mcp);
    }
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Quickwit ingest endpoint, e.g. "http://localhost:7280"
    pub quickwit_url: Option<String>,
    /// Quickwit index ID (for audit_logs)
    pub quickwit_index: String,
    /// Optional bearer token for Quickwit authentication
    pub quickwit_bearer_token: Option<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            quickwit_url: None,
            quickwit_index: "audit_logs".into(),
            quickwit_bearer_token: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Forwarder runtime state — one per active forwarder row
// ---------------------------------------------------------------------------

struct ForwarderRuntime {
    config: LogForwarder,
    udp_socket: Option<UdpSocket>,
}

/// Shared forwarder registry, reloaded periodically from the database.
type ForwarderRegistry = Arc<RwLock<HashMap<Uuid, ForwarderRuntime>>>;

/// Async audit log dispatcher. Receives entries via a bounded channel,
/// writes to Quickwit + DB-configured forwarders (syslog, kafka, webhook).
#[derive(Clone)]
pub struct AuditLogger {
    tx: mpsc::Sender<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
}

impl AuditLogger {
    pub fn new(config: AuditConfig, db: Option<PgPool>) -> Self {
        let (tx, rx) = mpsc::channel(10_000);
        let registry: ForwarderRegistry = Arc::new(RwLock::new(HashMap::new()));

        // Spawn the background worker
        tokio::spawn(audit_worker(config, rx, db.clone(), registry.clone()));

        // Spawn periodic forwarder reload (every 10s)
        if let Some(pool) = &db {
            let pool = pool.clone();
            let reg = registry.clone();
            tokio::spawn(async move {
                reload_forwarders_loop(pool, reg).await;
            });
        }

        Self { tx, db, registry }
    }

    pub fn log(&self, entry: AuditEntry) {
        if let Err(e) = self.tx.try_send(entry) {
            tracing::warn!("Audit log channel send failed (buffer full or closed): {e}");
        }
    }

    /// Force-reload forwarder configs from DB (called after CRUD ops).
    pub async fn reload_forwarders(&self) {
        if let Some(ref db) = self.db {
            reload_forwarders(db, &self.registry).await;
        }
    }
}

/// Periodically reload forwarder configs from the database.
async fn reload_forwarders_loop(db: PgPool, registry: ForwarderRegistry) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        reload_forwarders(&db, &registry).await;
    }
}

async fn reload_forwarders(db: &PgPool, registry: &ForwarderRegistry) {
    let rows = match sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders")
        .fetch_all(db)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to load log forwarders from DB: {e}");
            return;
        }
    };

    let mut map = HashMap::new();
    for row in rows {
        let udp_socket = if row.forwarder_type == "udp_syslog" && row.enabled {
            UdpSocket::bind("0.0.0.0:0").ok()
        } else {
            None
        };
        map.insert(
            row.id,
            ForwarderRuntime {
                config: row,
                udp_socket,
            },
        );
    }

    let mut guard = registry.write().await;
    *guard = map;
}

// ---------------------------------------------------------------------------
// Background worker
// ---------------------------------------------------------------------------

async fn audit_worker(
    config: AuditConfig,
    mut rx: mpsc::Receiver<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
) {
    let http_client = reqwest::Client::new();

    // Separate batches per log type for routing to correct Quickwit index
    let mut batches: HashMap<&'static str, Vec<AuditEntry>> = HashMap::new();
    let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                // Forward to all enabled forwarders immediately
                forward_to_all(&http_client, &registry, &db, &entry).await;

                let index_id = entry.log_type.index_id();
                let batch = batches.entry(index_id).or_insert_with(|| Vec::with_capacity(64));
                batch.push(entry);
                if batch.len() >= 50 {
                    let mut b = std::mem::take(batch);
                    flush_to_quickwit(&http_client, &config, index_id, &mut b).await;
                }
            }
            _ = flush_interval.tick() => {
                for (index_id, batch) in batches.iter_mut() {
                    if !batch.is_empty() {
                        flush_to_quickwit(&http_client, &config, index_id, batch).await;
                    }
                }
            }
            else => break,
        }
    }
}

/// Forward a single entry to all enabled forwarders that match the log type.
async fn forward_to_all(
    http_client: &reqwest::Client,
    registry: &ForwarderRegistry,
    db: &Option<PgPool>,
    entry: &AuditEntry,
) {
    let log_type_str = entry.log_type.as_str();
    let guard = registry.read().await;
    for (id, runtime) in guard.iter() {
        if !runtime.config.enabled {
            continue;
        }
        // Only forward if the forwarder subscribes to this log type
        if !runtime.config.log_types.iter().any(|t| t == log_type_str) {
            continue;
        }
        let result = match runtime.config.forwarder_type.as_str() {
            "udp_syslog" => send_udp_syslog(runtime, entry),
            "tcp_syslog" => send_tcp_syslog(&runtime.config, entry).await,
            "kafka" => send_kafka(http_client, &runtime.config, entry).await,
            "webhook" => send_webhook(http_client, &runtime.config, entry).await,
            other => {
                tracing::warn!("Unknown forwarder type: {other}");
                Err(format!("Unknown forwarder type: {other}"))
            }
        };

        // Update stats in DB (fire-and-forget)
        if let Some(pool) = &db {
            match result {
                Ok(()) => {
                    let _ = sqlx::query(
                        "UPDATE log_forwarders SET sent_count = sent_count + 1, last_sent_at = now(), updated_at = now() WHERE id = $1"
                    )
                    .bind(id)
                    .execute(pool)
                    .await;
                }
                Err(ref err_msg) => {
                    let _ = sqlx::query(
                        "UPDATE log_forwarders SET error_count = error_count + 1, last_error = $2, updated_at = now() WHERE id = $1"
                    )
                    .bind(id)
                    .bind(err_msg)
                    .execute(pool)
                    .await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Forwarder implementations
// ---------------------------------------------------------------------------

fn send_udp_syslog(runtime: &ForwarderRuntime, entry: &AuditEntry) -> Result<(), String> {
    let addr = runtime
        .config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in udp_syslog config")?;
    let facility: u8 = runtime
        .config
        .config
        .get("facility")
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(16); // default: local0

    let socket = runtime
        .udp_socket
        .as_ref()
        .ok_or("UDP socket not initialized")?;

    let severity = 6u8; // informational
    let priority = facility * 8 + severity;
    let hostname = "agent-bastion";
    let app_name = "audit";
    let msg_id = &entry.action;

    let structured_data = format!(
        "[audit@0 user_id=\"{}\" action=\"{}\" resource=\"{}\" ip=\"{}\"]",
        entry.user_id.as_deref().unwrap_or("-"),
        entry.action,
        entry.resource.as_deref().unwrap_or("-"),
        entry.ip_address.as_deref().unwrap_or("-"),
    );

    let message = format!(
        "<{priority}>1 {timestamp} {hostname} {app_name} - {msg_id} {structured_data} {action} on {resource}",
        priority = priority,
        timestamp = &entry.created_at,
        hostname = hostname,
        app_name = app_name,
        msg_id = msg_id,
        structured_data = structured_data,
        action = entry.action,
        resource = entry.resource.as_deref().unwrap_or("-"),
    );

    socket
        .send_to(message.as_bytes(), addr)
        .map(|_| ())
        .map_err(|e| format!("Syslog UDP send failed: {e}"))
}

async fn send_tcp_syslog(config: &LogForwarder, entry: &AuditEntry) -> Result<(), String> {
    let addr = config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in tcp_syslog config")?;
    let facility: u8 = config
        .config
        .get("facility")
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(16);

    let severity = 6u8;
    let priority = facility * 8 + severity;

    let structured_data = format!(
        "[audit@0 user_id=\"{}\" action=\"{}\" resource=\"{}\" ip=\"{}\"]",
        entry.user_id.as_deref().unwrap_or("-"),
        entry.action,
        entry.resource.as_deref().unwrap_or("-"),
        entry.ip_address.as_deref().unwrap_or("-"),
    );

    let message = format!(
        "<{priority}>1 {ts} agent-bastion audit - {action} {sd} {action} on {resource}\n",
        priority = priority,
        ts = &entry.created_at,
        action = entry.action,
        sd = structured_data,
        resource = entry.resource.as_deref().unwrap_or("-"),
    );

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("TCP syslog connect failed: {e}"))?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, message.as_bytes())
        .await
        .map_err(|e| format!("TCP syslog write failed: {e}"))
}

async fn send_kafka(
    client: &reqwest::Client,
    config: &LogForwarder,
    entry: &AuditEntry,
) -> Result<(), String> {
    // Kafka via REST proxy (Confluent-compatible)
    let broker_url = config
        .config
        .get("broker_url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'broker_url' in kafka config")?;
    let topic = config
        .config
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'topic' in kafka config")?;

    let payload = serde_json::json!({
        "records": [{
            "value": entry
        }]
    });

    let url = format!("{}/topics/{}", broker_url.trim_end_matches('/'), topic);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/vnd.kafka.json.v2+json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Kafka REST proxy request failed: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Kafka REST proxy returned error: {body}"))
    }
}

async fn send_webhook(
    client: &reqwest::Client,
    config: &LogForwarder,
    entry: &AuditEntry,
) -> Result<(), String> {
    let url = config
        .config
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'url' in webhook config")?;

    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(entry);

    // Optional auth header
    if let Some(token) = config.config.get("auth_header").and_then(|v| v.as_str()) {
        req = req.header("Authorization", token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Webhook request failed: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Webhook returned {status}: {body}"))
    }
}

// ---------------------------------------------------------------------------
// Quickwit (unchanged)
// ---------------------------------------------------------------------------

fn sanitize_detail(detail: &mut Option<serde_json::Value>) {
    if let Some(serde_json::Value::Object(map)) = detail {
        let keys_to_redact: Vec<String> = map
            .keys()
            .filter(|k| {
                let lower = k.to_lowercase();
                lower.contains("password")
                    || lower.contains("secret")
                    || lower.contains("token")
                    || lower.contains("key")
                    || lower.contains("authorization")
                    || lower.contains("credential")
            })
            .cloned()
            .collect();
        for key in keys_to_redact {
            map.insert(key, serde_json::Value::String("[REDACTED]".to_string()));
        }
    }
}

async fn flush_to_quickwit(
    client: &reqwest::Client,
    config: &AuditConfig,
    index_id: &str,
    batch: &mut Vec<AuditEntry>,
) {
    let Some(ref url) = config.quickwit_url else {
        batch.clear();
        return;
    };

    for entry in batch.iter_mut() {
        sanitize_detail(&mut entry.detail);
    }

    let ndjson: String = batch
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect::<Vec<_>>()
        .join("\n");

    let ingest_url = format!("{}/api/v1/{}/ingest", url, index_id);

    const MAX_RETRIES: u32 = 3;
    for attempt in 1..=MAX_RETRIES {
        let mut req = client
            .post(&ingest_url)
            .header("Content-Type", "application/x-ndjson")
            .body(ndjson.clone());
        if let Some(ref token) = config.quickwit_bearer_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(
                    "Flushed {} entries to Quickwit index {}",
                    batch.len(),
                    index_id
                );
                batch.clear();
                return;
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if attempt < MAX_RETRIES {
                    tracing::warn!(
                        "Quickwit ingest returned {status} (attempt {attempt}/{MAX_RETRIES}): {body}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                        .await;
                } else {
                    tracing::error!(
                        "Quickwit ingest failed after {MAX_RETRIES} retries ({status}): {body} — dropping {} entries",
                        batch.len()
                    );
                }
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    tracing::warn!("Quickwit ingest error (attempt {attempt}/{MAX_RETRIES}): {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                        .await;
                } else {
                    tracing::error!(
                        "Quickwit ingest failed after {MAX_RETRIES} retries: {e} — dropping {} entries",
                        batch.len()
                    );
                }
            }
        }
    }

    batch.clear();
}

/// Initialize the Quickwit index (create if not exists). Call once at startup.
pub async fn ensure_quickwit_index(
    quickwit_url: &str,
    _index_id: &str,
    bearer_token: Option<&str>,
) {
    let client = reqwest::Client::new();

    let indices: &[(&str, &str)] = &[
        (
            "audit_logs",
            include_str!("../../../deploy/quickwit/audit_logs_index.yaml"),
        ),
        (
            "gateway_logs",
            include_str!("../../../deploy/quickwit/gateway_logs_index.yaml"),
        ),
        (
            "mcp_logs",
            include_str!("../../../deploy/quickwit/mcp_logs_index.yaml"),
        ),
        (
            "platform_logs",
            include_str!("../../../deploy/quickwit/platform_logs_index.yaml"),
        ),
    ];

    for (index_id, index_config) in indices {
        let check_url = format!("{}/api/v1/indexes/{}", quickwit_url, index_id);
        let mut req = client.get(&check_url);
        if let Some(token) = bearer_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("Quickwit index '{index_id}' already exists");
                continue;
            }
            _ => {}
        }

        let create_url = format!("{}/api/v1/indexes", quickwit_url);
        let mut req = client
            .post(&create_url)
            .header("Content-Type", "application/yaml")
            .body(index_config.to_string());
        if let Some(token) = bearer_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("Created Quickwit index '{index_id}'");
            }
            Ok(resp) => {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!("Failed to create Quickwit index '{index_id}': {body}");
            }
            Err(e) => {
                tracing::warn!("Failed to connect to Quickwit for index '{index_id}': {e}");
            }
        }
    }
}
