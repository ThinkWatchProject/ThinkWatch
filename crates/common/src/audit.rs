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
// Log types — each maps to a distinct ClickHouse table
// ---------------------------------------------------------------------------

/// The category of a log entry, determining which ClickHouse table it's stored in
/// and which forwarders will receive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogType {
    /// HTTP access log (both gateway & console)
    Access,
    /// Runtime application logs (info/warn/error/debug)
    App,
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
            LogType::Access => "access",
            LogType::App => "app",
            LogType::Audit => "audit",
            LogType::Gateway => "gateway",
            LogType::Mcp => "mcp",
            LogType::Platform => "platform",
        }
    }

    pub fn index_id(&self) -> &'static str {
        match self {
            LogType::Access => "access_logs",
            LogType::App => "app_logs",
            LogType::Audit => "audit_logs",
            LogType::Gateway => "gateway_logs",
            LogType::Mcp => "mcp_logs",
            LogType::Platform => "platform_logs",
        }
    }
}

/// Audit log entry sent to ClickHouse and dynamically configured forwarders.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: String,
    #[serde(skip)]
    pub log_type: LogType,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub api_key_id: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub resource_id: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    /// Correlates this event with other gateway/mcp/audit rows that
    /// belong to the same incoming request. Typically set to the
    /// gateway's `metadata.request_id`; `None` for standalone admin
    /// actions where there is no request to correlate against.
    pub trace_id: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Per-table Row structs for ClickHouse SDK insert
// ---------------------------------------------------------------------------

/// audit_logs — API key usage audit
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChAuditRow {
    id: String,
    user_id: Option<String>,
    user_email: Option<String>,
    api_key_id: Option<String>,
    action: String,
    resource: Option<String>,
    resource_id: Option<String>,
    detail: Option<String>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    // Order matches CREATE TABLE in deploy/clickhouse/init.sql; trace_id
    // must sit here (between user_agent and created_at) so CH's columnar
    // insert lines up. If you move one, move both.
    trace_id: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// platform_logs — admin/management operations (no api_key_id)
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChPlatformRow {
    id: String,
    user_id: Option<String>,
    user_email: Option<String>,
    action: String,
    resource: Option<String>,
    resource_id: Option<String>,
    detail: Option<String>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// gateway_logs — model request logs
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChGatewayRow {
    id: String,
    user_id: Option<String>,
    api_key_id: Option<String>,
    model_id: Option<String>,
    provider: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost_usd: Option<f64>,
    latency_ms: Option<i64>,
    status_code: Option<i64>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    detail: Option<String>,
    trace_id: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// mcp_logs — MCP tool invocation logs
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChMcpRow {
    id: String,
    user_id: Option<String>,
    server_id: Option<String>,
    server_name: Option<String>,
    tool_name: Option<String>,
    duration_ms: Option<i64>,
    status: Option<String>,
    error_message: Option<String>,
    ip_address: Option<String>,
    detail: Option<String>,
    trace_id: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// app_logs — runtime tracing logs
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChAppLogRow {
    id: String,
    level: String,
    target: String,
    message: String,
    fields: Option<String>,
    span: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// access_logs — HTTP access log
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChAccessRow {
    id: String,
    method: String,
    path: String,
    status_code: u16,
    latency_ms: i64,
    port: u16,
    user_id: Option<String>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    created_at: chrono::DateTime<Utc>,
}

/// Parse RFC3339 string to DateTime<Utc>, fallback to now.
fn parse_created_at(s: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn detail_str(detail: &mut Option<serde_json::Value>) -> Option<String> {
    sanitize_detail(detail);
    detail.as_ref().map(|v| v.to_string())
}

fn detail_field<T: serde::de::DeserializeOwned>(
    detail: &Option<serde_json::Value>,
    key: &str,
) -> Option<T> {
    detail
        .as_ref()?
        .get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

impl AuditEntry {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            log_type: LogType::Audit,
            user_id: None,
            user_email: None,
            api_key_id: None,
            action: action.into(),
            resource: None,
            resource_id: None,
            detail: None,
            ip_address: None,
            user_agent: None,
            trace_id: None,
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

    pub fn user_email(mut self, email: impl Into<String>) -> Self {
        self.user_email = Some(email.into());
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

    /// Correlate this row with other events for the same request.
    /// Typically the gateway's `metadata.request_id`. Omit for admin
    /// actions that aren't tied to a gateway call.
    pub fn trace_id(mut self, id: impl Into<String>) -> Self {
        self.trace_id = Some(id.into());
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

    #[test]
    fn trace_id_builder_sets_field() {
        let entry = AuditEntry::new("some.action").trace_id("abc-123");
        assert_eq!(entry.trace_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn hmac_sha256_hex_matches_rfc4231_vector() {
        // RFC 4231 Test Case 1: key = 20 bytes of 0x0b, data = "Hi There".
        // Expected HMAC-SHA256: b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
        let key = [0x0b; 20];
        let got = hmac_sha256_hex(&key, b"Hi There");
        assert_eq!(
            got,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_hex_is_deterministic_and_secret_dependent() {
        let a = hmac_sha256_hex(b"secret-a", b"payload");
        let b = hmac_sha256_hex(b"secret-a", b"payload");
        let c = hmac_sha256_hex(b"secret-b", b"payload");
        assert_eq!(a, b, "same secret + payload must be stable");
        assert_ne!(a, c, "different secret must change the digest");
    }
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// ClickHouse HTTP endpoint, e.g. "http://localhost:8123"
    pub clickhouse_url: Option<String>,
    /// ClickHouse database name
    pub clickhouse_db: String,
    /// ClickHouse user for authentication
    pub clickhouse_user: Option<String>,
    /// ClickHouse password for authentication
    pub clickhouse_password: Option<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            clickhouse_url: None,
            clickhouse_db: "think_watch".into(),
            clickhouse_user: None,
            clickhouse_password: None,
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
/// writes to ClickHouse + DB-configured forwarders (syslog, kafka, webhook).
#[derive(Clone)]
pub struct AuditLogger {
    tx: mpsc::Sender<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
}

/// Bounded audit channel capacity.
///
/// At ~80 bytes per entry that's about 8 MB of in-memory backlog,
/// which is fine for any reasonable host. The previous 10k bound
/// was too tight: a few seconds of ClickHouse stalling at 10k req/s
/// silently dropped audit data. Bump to 100k so a 30-second outage
/// at 3k req/s still survives, and surface drops via a metric
/// instead of just logging.
const AUDIT_CHANNEL_CAPACITY: usize = 100_000;

/// Throttle window for the structured "audit drop" log line. The
/// metric still increments on every drop, so dashboards see the
/// real rate; the log is just for human inspection and one line
/// per second is plenty.
fn log_audit_drop_throttled(err: &tokio::sync::mpsc::error::TrySendError<AuditEntry>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST_LOG_SECS: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_LOG_SECS.load(Ordering::Relaxed);
    if now != last
        && LAST_LOG_SECS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::error!(
            "Audit log channel send failed (buffer full or closed): {err} \
             — see audit_log_dropped_total / audit_log_queue_depth metrics"
        );
    }
}

impl AuditLogger {
    pub fn new(_config: AuditConfig, db: Option<PgPool>, ch: Option<clickhouse::Client>) -> Self {
        let (tx, rx) = mpsc::channel(AUDIT_CHANNEL_CAPACITY);
        let registry: ForwarderRegistry = Arc::new(RwLock::new(HashMap::new()));

        // Spawn the background worker
        tokio::spawn(audit_worker(ch, rx, db.clone(), registry.clone()));

        // Spawn periodic forwarder reload (every 10s)
        if let Some(pool) = &db {
            let pool = pool.clone();
            let reg = registry.clone();
            tokio::spawn(async move {
                reload_forwarders_loop(pool, reg).await;
            });
        }

        // Spawn a queue-depth sampler that updates the
        // `audit_log_queue_depth` gauge every second. Without this,
        // operators only see backlog after `audit_log_dropped_total`
        // has already started incrementing — by which point data
        // is already gone. The gauge gives a leading indicator that
        // ClickHouse / forwarders are getting behind.
        {
            let tx_for_gauge = tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    // `tokio::sync::mpsc::Sender` doesn't expose
                    // current length directly; capacity() returns
                    // the REMAINING capacity, so depth = total - capacity.
                    let depth = AUDIT_CHANNEL_CAPACITY.saturating_sub(tx_for_gauge.capacity());
                    metrics::gauge!("audit_log_queue_depth").set(depth as f64);
                }
            });
        }

        Self { tx, db, registry }
    }

    pub fn log(&self, entry: AuditEntry) {
        if let Err(e) = self.tx.try_send(entry) {
            // Compliance signal: dropped audit entries are a real
            // operational incident, not a debug warning. Bump the
            // metric on every drop so dashboards / alerts see the
            // true rate, but throttle the structured log to once
            // per second — at 10k req/s a sustained drop would
            // otherwise produce 10k error lines/sec and saturate
            // the log pipeline along with the audit pipeline.
            metrics::counter!("audit_log_dropped_total").increment(1);
            log_audit_drop_throttled(&e);
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
    ch: Option<clickhouse::Client>,
    mut rx: mpsc::Receiver<AuditEntry>,
    db: Option<PgPool>,
    registry: ForwarderRegistry,
) {
    let http_client = reqwest::Client::new();

    // Separate batches per log type for routing to correct ClickHouse table
    let mut batches: HashMap<&'static str, Vec<AuditEntry>> = HashMap::new();
    let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                // Forward to all enabled forwarders immediately
                forward_to_all(&http_client, &registry, &db, &entry).await;

                let table = entry.log_type.index_id();
                let batch = batches.entry(table).or_insert_with(|| Vec::with_capacity(64));
                batch.push(entry);
                if batch.len() >= 50 {
                    let mut b = std::mem::take(batch);
                    flush_to_clickhouse(&ch, table, &mut b).await;
                }
            }
            _ = flush_interval.tick() => {
                for (table, batch) in batches.iter_mut() {
                    if !batch.is_empty() {
                        flush_to_clickhouse(&ch, table, batch).await;
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
    let hostname = "think-watch";
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
        "<{priority}>1 {ts} think-watch audit - {action} {sd} {action} on {resource}\n",
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

    // Serialize the body once so the HMAC signs exactly what goes over
    // the wire — avoids any field-ordering or whitespace divergence
    // between the signature input and the posted body.
    let body = serde_json::to_vec(entry).map_err(|e| format!("JSON serialise: {e}"))?;

    // Optional HMAC-SHA256 signature. When `signing_secret` is set on
    // the forwarder row, every delivery gets an `x-signature` header
    // with `sha256=<hex>` over the body bytes. Receivers can verify by
    // recomputing with the same secret; a mismatch means the payload
    // was tampered with in transit (or arrived via a different sender).
    let signing_secret = config
        .config
        .get("signing_secret")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let signature = signing_secret.map(|secret| hmac_sha256_hex(secret.as_bytes(), &body));

    // Retry policy: 3 attempts, exponential backoff (200ms, 400ms,
    // 800ms). A 2xx or explicit 4xx terminates — 4xx is a config
    // problem at the receiver, retrying would just amplify noise.
    // Network errors and 5xx retry up to the attempt cap.
    let mut last_err = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            let delay_ms = 200u64 * (1u64 << (attempt - 1)); // 200, 400, 800
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.clone());

        // Custom headers (new format: JSON object stored as string)
        if let Some(headers_val) = config.config.get("custom_headers") {
            let headers_str = headers_val.as_str().unwrap_or("");
            if let Ok(headers) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_str)
            {
                for (k, v) in headers {
                    if let Some(v_str) = v.as_str() {
                        req = req.header(k.as_str(), v_str);
                    }
                }
            }
        }

        // Legacy: single auth_header field
        if let Some(token) = config.config.get("auth_header").and_then(|v| v.as_str()) {
            req = req.header("Authorization", token);
        }

        if let Some(ref sig) = signature {
            req = req.header("x-signature", format!("sha256={sig}"));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let rtext = resp.text().await.unwrap_or_default();
                last_err = format!("Webhook returned {status}: {rtext}");
                if status.is_client_error() {
                    // 4xx — no point retrying, the receiver rejected
                    // the payload shape itself.
                    return Err(last_err);
                }
            }
            Err(e) => {
                last_err = format!("Webhook request failed: {e}");
            }
        }
    }
    Err(last_err)
}

/// Hex-encoded HMAC-SHA256. Kept local to this module so the forwarder
/// doesn't pull in a hmac-crate dependency on the shared `common` crate.
/// Implemented via the existing `hmac` workspace dep, re-exported from
/// the auth crate is undesirable (common should not depend on auth).
fn hmac_sha256_hex(secret: &[u8], msg: &[u8]) -> String {
    use hmac::{Hmac, Mac, digest::KeyInit};
    type HmacSha256 = Hmac<sha2::Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    tag.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// ClickHouse ingest
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

async fn flush_to_clickhouse(
    ch: &Option<clickhouse::Client>,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) {
    let Some(client) = ch else {
        batch.clear();
        return;
    };

    let count = batch.len();

    // Determine log type from first entry (all entries in a batch share the same table)
    let log_type = batch.first().map(|e| e.log_type);
    let result = match log_type {
        Some(LogType::Access) => flush_access(client, table, batch).await,
        Some(LogType::App) => flush_app(client, table, batch).await,
        Some(LogType::Audit) => flush_audit(client, table, batch).await,
        Some(LogType::Platform) => flush_platform(client, table, batch).await,
        Some(LogType::Gateway) => flush_gateway(client, table, batch).await,
        Some(LogType::Mcp) => flush_mcp(client, table, batch).await,
        None => {
            batch.clear();
            return;
        }
    };

    match result {
        Ok(()) => tracing::debug!("Flushed {count} entries to ClickHouse table {table}"),
        Err(e) => {
            tracing::error!("ClickHouse insert failed for {table}: {e} — dropped {count} entries")
        }
    }
    batch.clear();
}

async fn flush_app(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAppLogRow>(table)?;
    for entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAppLogRow {
                id: entry.id,
                level: entry.action, // we store level in action field
                target: entry.resource.unwrap_or_default(),
                message: entry.resource_id.unwrap_or_default(),
                fields: entry.detail.map(|v| v.to_string()),
                span: entry.user_agent, // repurpose user_agent for span info
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_access(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAccessRow>(table)?;
    for entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAccessRow {
                id: entry.id,
                method: detail_field(&entry.detail, "method").unwrap_or_default(),
                path: detail_field(&entry.detail, "path").unwrap_or_default(),
                status_code: detail_field(&entry.detail, "status_code").unwrap_or(0),
                latency_ms: detail_field(&entry.detail, "latency_ms").unwrap_or(0),
                port: detail_field(&entry.detail, "port").unwrap_or(0),
                user_id: entry.user_id,
                ip_address: entry.ip_address,
                user_agent: entry.user_agent,
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_audit(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAuditRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAuditRow {
                id: entry.id,
                user_id: entry.user_id,
                user_email: entry.user_email,
                api_key_id: entry.api_key_id,
                action: entry.action,
                resource: entry.resource,
                resource_id: entry.resource_id,
                detail: detail_str(&mut entry.detail),
                ip_address: entry.ip_address,
                user_agent: entry.user_agent,
                trace_id: entry.trace_id,
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_platform(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChPlatformRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChPlatformRow {
                id: entry.id,
                user_id: entry.user_id,
                user_email: entry.user_email,
                action: entry.action,
                resource: entry.resource,
                resource_id: entry.resource_id,
                detail: detail_str(&mut entry.detail),
                ip_address: entry.ip_address,
                user_agent: entry.user_agent,
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_gateway(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChGatewayRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        let row = ChGatewayRow {
            id: entry.id,
            user_id: entry.user_id,
            api_key_id: entry.api_key_id,
            model_id: detail_field(&entry.detail, "model_id"),
            provider: detail_field(&entry.detail, "provider"),
            input_tokens: detail_field(&entry.detail, "input_tokens"),
            output_tokens: detail_field(&entry.detail, "output_tokens"),
            cost_usd: detail_field(&entry.detail, "cost_usd"),
            latency_ms: detail_field(&entry.detail, "latency_ms"),
            status_code: detail_field(&entry.detail, "status_code"),
            ip_address: entry.ip_address,
            user_agent: entry.user_agent,
            detail: detail_str(&mut entry.detail),
            trace_id: entry.trace_id,
            created_at: ts,
        };
        insert.write(&row).await?;
    }
    insert.end().await
}

async fn flush_mcp(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChMcpRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        let row = ChMcpRow {
            id: entry.id,
            user_id: entry.user_id,
            server_id: detail_field(&entry.detail, "server_id"),
            server_name: detail_field(&entry.detail, "server_name"),
            tool_name: detail_field(&entry.detail, "tool_name"),
            duration_ms: detail_field(&entry.detail, "duration_ms"),
            status: detail_field(&entry.detail, "status"),
            error_message: detail_field(&entry.detail, "error_message"),
            ip_address: entry.ip_address,
            detail: detail_str(&mut entry.detail),
            trace_id: entry.trace_id,
            created_at: ts,
        };
        insert.write(&row).await?;
    }
    insert.end().await
}

/// Ensure ClickHouse tables exist. Call once at startup.
/// Run the ClickHouse `init.sql` schema bootstrap exactly once at
/// startup.
///
/// Returns `Ok(())` when ClickHouse isn't configured at all
/// (`ch = None` is a valid deployment — operators who don't want
/// columnar audit can opt out via env), or when every CREATE
/// statement succeeded. Returns `Err` on the first failed statement
/// so the caller can retry with backoff and refuse to start the
/// gateway if the database is permanently unreachable.
pub async fn ensure_clickhouse_tables(
    ch: &Option<clickhouse::Client>,
) -> Result<(), clickhouse::error::Error> {
    let Some(client) = ch else {
        return Ok(());
    };

    let init_sql = include_str!("../../../deploy/clickhouse/init.sql");

    for statement in init_sql.split(';') {
        let stmt = statement.trim();
        if stmt.is_empty() || stmt.starts_with("--") {
            continue;
        }

        if let Err(e) = client.query(stmt).execute().await {
            tracing::warn!("ClickHouse init statement failed: {e}");
            return Err(e);
        }
    }

    // One-shot backfill of aggregate tables. The MVs attached to
    // mcp_logs / gateway_logs only capture rows inserted *after* the
    // MV is created, so on first boot (or when schema is upgraded to
    // include these MVs) we seed the aggregate tables with a snapshot
    // of whatever history is still retained. Gated on emptiness so it
    // runs exactly once per aggregate — cheap to check, safe to skip.
    backfill_if_empty(
        client,
        "mcp_server_call_counts",
        "INSERT INTO mcp_server_call_counts \
         SELECT server_id, toUInt64(count()) AS calls \
         FROM mcp_logs WHERE server_id IS NOT NULL GROUP BY server_id",
    )
    .await;
    backfill_if_empty(
        client,
        "provider_health_5m",
        "INSERT INTO provider_health_5m \
         SELECT toStartOfFiveMinutes(created_at) AS bucket_5m, \
                provider, \
                toUInt64(count()) AS total_requests, \
                toUInt64(countIf(status_code >= 400)) AS error_requests, \
                sum(ifNull(latency_ms, 0)) AS sum_latency_ms, \
                toUInt64(countIf(latency_ms IS NOT NULL)) AS requests_latency \
         FROM gateway_logs WHERE provider IS NOT NULL \
         GROUP BY bucket_5m, provider",
    )
    .await;

    tracing::info!("ClickHouse tables initialized");
    Ok(())
}

/// Run `insert_sql` against `client` iff `table` is currently empty.
/// Failures are logged but not propagated — a missing backfill yields
/// a temporarily-low metric, never a failed boot.
async fn backfill_if_empty(client: &clickhouse::Client, table: &str, insert_sql: &str) {
    let count_sql = format!("SELECT count() FROM {table}");
    match client.query(&count_sql).fetch_one::<u64>().await {
        Ok(0) => {
            if let Err(e) = client.query(insert_sql).execute().await {
                tracing::warn!("ClickHouse backfill of {table} failed: {e}");
            } else {
                tracing::info!("ClickHouse aggregate {table} backfilled from source log table");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("ClickHouse count({table}) failed: {e}"),
    }
}
