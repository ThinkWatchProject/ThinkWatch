use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock, mpsc};
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

/// `log_type` deserialisation default used when an outbox row was
/// produced before the field was added — the webhook payload omits
/// it via `serde(skip)` either way, so any drained row needs a fresh
/// default to keep the engine routing decisions sane.
fn default_log_type() -> LogType {
    LogType::Audit
}

/// Audit log entry sent to ClickHouse and dynamically configured forwarders.
///
/// Deserialize is required because the durable webhook outbox round-
/// trips entries through Postgres JSONB; the drain worker pulls them
/// back out and feeds them into `send_webhook` again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    #[serde(skip, default = "default_log_type")]
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
    // Order matches CREATE TABLE in deploy/clickhouse/initdb.d/01_init.sql; trace_id
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

/// gateway_logs — model request logs. `user_email` is a point-in-time
/// snapshot of the user's email — queries against historical rows stay
/// readable even after the user is hard-deleted.
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChGatewayRow {
    id: String,
    user_id: Option<String>,
    user_email: Option<String>,
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

/// mcp_logs — MCP tool invocation logs. `user_email` snapshotted as
/// in ChGatewayRow.
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChMcpRow {
    id: String,
    user_id: Option<String>,
    user_email: Option<String>,
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

/// access_logs — HTTP access log. `user_email` snapshotted as in ChGatewayRow.
#[derive(Debug, clickhouse::Row, Serialize)]
struct ChAccessRow {
    id: String,
    method: String,
    path: String,
    status_code: u16,
    latency_ms: i64,
    port: u16,
    user_id: Option<String>,
    user_email: Option<String>,
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

    #[test]
    fn outbox_backoff_doubles_then_caps_at_one_hour() {
        // Sanity: monotonic non-decreasing and exactly the documented
        // schedule for the first 8 attempts.
        assert_eq!(outbox_backoff_secs(1), 30);
        assert_eq!(outbox_backoff_secs(2), 60);
        assert_eq!(outbox_backoff_secs(3), 120);
        assert_eq!(outbox_backoff_secs(4), 240);
        assert_eq!(outbox_backoff_secs(5), 480);
        assert_eq!(outbox_backoff_secs(6), 960);
        assert_eq!(outbox_backoff_secs(7), 1920);
        // attempt 8 onwards: clamped at 1h.
        assert_eq!(outbox_backoff_secs(8), 3600);
        assert_eq!(outbox_backoff_secs(9), 3600);
        assert_eq!(outbox_backoff_secs(MAX_OUTBOX_ATTEMPTS - 1), 3600);
    }

    #[test]
    fn outbox_backoff_floors_attempt_number_at_one() {
        // Defensive: caller passing 0 or negative shouldn't underflow
        // the shift. Treat as attempt 1.
        assert_eq!(outbox_backoff_secs(0), 30);
        assert_eq!(outbox_backoff_secs(-5), 30);
    }

    #[test]
    fn outbox_backoff_total_max_lifetime_is_under_one_day() {
        // 24 attempts at the cap == ~24h — keeps the doc claim
        // ("dropped after ~1 day") honest. Exact upper bound:
        // 30 + 60 + 120 + 240 + 480 + 960 + 1920 + 16 × 3600.
        let total: u64 = (1..=MAX_OUTBOX_ATTEMPTS).map(outbox_backoff_secs).sum();
        // < 25 hours (86_400 s × 25 / 24 ≈ 90_000); roughly a day.
        assert!(total < 90_000, "total backoff window {total}s exceeds ~25h");
        assert!(
            total > 60_000,
            "total backoff window {total}s shorter than expected"
        );
    }

    /// AuditEntry must round-trip through JSONB: the durable webhook
    /// outbox stores entries serialized, and an old payload missing
    /// the `log_type` field (which is `#[serde(skip)]`) must still
    /// parse — that field gets re-defaulted by `default_log_type`.
    #[test]
    fn audit_entry_deserialise_minimal_payload() {
        // Minimal payload: id + action + created_at, everything else
        // None / default.
        let json = r#"{
            "id": "abc",
            "action": "test.event",
            "user_id": null,
            "user_email": null,
            "api_key_id": null,
            "resource": null,
            "resource_id": null,
            "detail": null,
            "ip_address": null,
            "user_agent": null,
            "trace_id": null,
            "created_at": "2026-04-15T00:00:00Z"
        }"#;
        let entry: AuditEntry = serde_json::from_str(json).expect("parse minimal payload");
        assert_eq!(entry.id, "abc");
        assert_eq!(entry.action, "test.event");
        // log_type was missing from the wire → falls back to default.
        assert_eq!(entry.log_type, LogType::Audit);
    }

    #[test]
    fn audit_entry_round_trip_through_json() {
        let original = AuditEntry::new("round.trip")
            .resource("foo:1")
            .trace_id("trace-xyz")
            .detail(serde_json::json!({"k": "v"}));
        let bytes = serde_json::to_vec(&original).expect("serialise");
        let decoded: AuditEntry = serde_json::from_slice(&bytes).expect("deserialise");
        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.action, original.action);
        assert_eq!(decoded.resource, original.resource);
        assert_eq!(decoded.trace_id, original.trace_id);
        assert_eq!(decoded.detail, original.detail);
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
    tcp_stream: Arc<Mutex<Option<tokio::net::TcpStream>>>,
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
    /// Sample rate in 1/10000ths (0 = drop everything, 10000 = keep
    /// everything). Stored as a `u32` so reads are lock-free on the
    /// hot logging path; written by the dynamic-config subscriber
    /// when `audit.sample_rate` changes.
    sample_rate_bps: Arc<std::sync::atomic::AtomicU32>,
}

/// Bounded audit channel capacity.
///
/// At ~80 bytes per entry that's about 8 MB of in-memory backlog,
/// which is fine for any reasonable host. 100k entries means a
/// 30-second ClickHouse outage at 3k req/s still survives; drops
/// are surfaced via a metric.
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
    pub async fn new(
        _config: AuditConfig,
        db: Option<PgPool>,
        ch: Option<clickhouse::Client>,
        dynamic_config: Option<Arc<crate::dynamic_config::DynamicConfig>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(AUDIT_CHANNEL_CAPACITY);
        let registry: ForwarderRegistry = Arc::new(RwLock::new(HashMap::new()));
        let sample_rate_bps = Arc::new(std::sync::atomic::AtomicU32::new(10_000));

        // Populate the forwarder registry BEFORE the worker starts
        // consuming audit entries. Without this, an audit log sent in
        // the first few milliseconds after AuditLogger::new() returns
        // would see an empty registry and skip forwarding — logs that
        // should have gone to syslog/Kafka/webhooks during bootstrap
        // would silently vanish.
        if let Some(pool) = &db {
            reload_forwarders(pool, &registry).await;
        }

        // Pick up the persisted sample rate once before the worker
        // sees any traffic, then poll periodically. The dynamic_config
        // subscriber on Redis already keeps the in-memory cache fresh,
        // so polling every 30s costs nothing more than an Arc clone.
        if let Some(dc) = dynamic_config.as_ref() {
            let initial = dc.audit_sample_rate().await;
            sample_rate_bps.store(
                (initial.clamp(0.0, 1.0) * 10_000.0).round() as u32,
                std::sync::atomic::Ordering::Relaxed,
            );
            let dc = dc.clone();
            let bps = sample_rate_bps.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let rate = dc.audit_sample_rate().await;
                    bps.store(
                        (rate.clamp(0.0, 1.0) * 10_000.0).round() as u32,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            });
        }

        // Spawn the background worker
        tokio::spawn(audit_worker(ch, rx, db.clone(), registry.clone()));

        // Spawn periodic forwarder reload (every 10s)
        if let Some(pool) = &db {
            let reload_pool = pool.clone();
            let reload_reg = registry.clone();
            tokio::spawn(async move {
                reload_forwarders_loop(reload_pool, reload_reg).await;
            });

            // Durable webhook redelivery — drains rows the inline
            // retry couldn't deliver. Same registry handle so it
            // sees forwarder config edits the operator makes via
            // the admin UI without a restart.
            let drain_pool = pool.clone();
            let drain_reg = registry.clone();
            tokio::spawn(async move {
                webhook_outbox_drain_loop(drain_pool, drain_reg).await;
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

        Self {
            tx,
            db,
            registry,
            sample_rate_bps,
        }
    }

    /// Update the audit sample rate (0.0..=1.0). Lower values drop a
    /// proportional fraction of entries at `log()` time before the
    /// channel send, so a high-volume deployment can spare CH without
    /// also starving the forwarder queue. Called by the dynamic-config
    /// subscriber on startup and on every `audit.sample_rate` update.
    pub fn set_sample_rate(&self, rate: f64) {
        let clamped = rate.clamp(0.0, 1.0);
        let bps = (clamped * 10_000.0).round() as u32;
        self.sample_rate_bps
            .store(bps, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn log(&self, entry: AuditEntry) {
        // Sampling: consult the atomic rate and skip the entry when a
        // uniform draw falls outside the keep window. A full keep
        // (10000) short-circuits the RNG so the hot path pays nothing
        // until an operator actually dials sampling down.
        let bps = self
            .sample_rate_bps
            .load(std::sync::atomic::Ordering::Relaxed);
        if bps < 10_000 && rand::random_range(0..10_000) >= bps {
            metrics::counter!("audit_log_sampled_out_total").increment(1);
            return;
        }
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
                tcp_stream: Arc::new(Mutex::new(None)),
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
            "tcp_syslog" => send_tcp_syslog(runtime, entry).await,
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
                    // Failed deliveries park in `webhook_outbox` so the
                    // background drain worker can keep trying after the
                    // inline 3x retry exhausted. Originally webhook-only;
                    // syslog and kafka transports also benefit from a
                    // safety net — a transient TCP RST or broker
                    // reconnect shouldn't silently lose the audit row,
                    // and the same drain pipeline already dispatches
                    // by forwarder_type so non-webhook entries replay
                    // through the right transport on retry.
                    if let Ok(payload_json) = serde_json::to_value(entry) {
                        let _ = sqlx::query(
                            "INSERT INTO webhook_outbox (forwarder_id, payload, last_error) \
                             VALUES ($1, $2, $3)",
                        )
                        .bind(id)
                        .bind(&payload_json)
                        .bind(err_msg)
                        .execute(pool)
                        .await;
                        metrics::counter!(
                            "forwarder_deadletter_total",
                            "transport" => runtime.config.forwarder_type.clone(),
                        )
                        .increment(1);
                    }
                }
            }
        }
    }
}

/// Background drain for `webhook_outbox`. Polls every 10s, picks up
/// to 100 due rows, attempts redelivery, deletes on success, bumps
/// attempt count + reschedules on failure. Caps backoff at one hour
/// and gives up after 24 attempts (~1 day worth of redelivery).
async fn webhook_outbox_drain_loop(db: PgPool, registry: ForwarderRegistry) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tracks when the backlog first crossed `OUTBOX_ALERT_THRESHOLD`.
    // Once it has stayed above the threshold for `OUTBOX_ALERT_AFTER`,
    // we emit an audit entry that the webhook forwarders pick up — and
    // arm a re-fire window so a chronic backlog isn't silent for hours.
    let mut over_threshold_since: Option<std::time::Instant> = None;
    let mut last_alert_at: Option<std::time::Instant> = None;
    loop {
        interval.tick().await;
        if let Err(e) = drain_once(
            &db,
            &registry,
            &http,
            &mut over_threshold_since,
            &mut last_alert_at,
        )
        .await
        {
            tracing::warn!("webhook_outbox drain failed: {e}");
        }
    }
}

/// Threshold and dwell-time for the "outbox is backed up" audit alert.
const OUTBOX_ALERT_THRESHOLD: i64 = 100;
const OUTBOX_ALERT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);
const OUTBOX_ALERT_REPEAT: std::time::Duration = std::time::Duration::from_secs(900);

const MAX_OUTBOX_ATTEMPTS: i32 = 24;

/// Compute the next-attempt backoff (in seconds) for a webhook outbox
/// row that just failed `attempt_number` times (1-indexed: first
/// retry is attempt 1). Doubles every attempt, capped at 1 hour so
/// a long-broken receiver doesn't rot in the table for days between
/// attempts. Extracted so the schedule is unit-testable without
/// standing up a Postgres fixture.
pub(crate) fn outbox_backoff_secs(attempt_number: i32) -> u64 {
    let n = attempt_number.max(1) as u32;
    // Saturating shift caps the doubling at attempt 8 → 30 × 128 = 3840s,
    // then clamped to 3600. Anything past attempt 8 stays at 1h.
    let exp = (n - 1).min(7);
    (30u64.saturating_mul(1u64 << exp)).min(3600)
}

async fn drain_once(
    db: &PgPool,
    registry: &ForwarderRegistry,
    http: &reqwest::Client,
    over_threshold_since: &mut Option<std::time::Instant>,
    last_alert_at: &mut Option<std::time::Instant>,
) -> Result<(), sqlx::Error> {
    // Surface the backlog depth every tick so operators can alert on
    // "outbox > N rows for M minutes". Published before the drain so
    // the gauge reflects the pre-drain snapshot the tick operated on.
    let depth: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_outbox")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    metrics::gauge!("webhook_outbox_depth").set(depth as f64);

    // Sustained-backlog alert. The gauge alone tells dashboards what's
    // happening; the audit entry below routes the same signal through
    // the existing webhook forwarders so on-call gets paged without
    // wiring up Prometheus → Alertmanager separately. Forwarders pick
    // it up by `action = "alert.outbox_depth_high"`.
    let now = std::time::Instant::now();
    if depth >= OUTBOX_ALERT_THRESHOLD {
        let crossed_at = *over_threshold_since.get_or_insert(now);
        let dwell = now.duration_since(crossed_at);
        let due_for_first_alert = last_alert_at.is_none() && dwell >= OUTBOX_ALERT_AFTER;
        let due_for_repeat = last_alert_at
            .map(|t| now.duration_since(t) >= OUTBOX_ALERT_REPEAT)
            .unwrap_or(false);
        if due_for_first_alert || due_for_repeat {
            // Forwarder dispatch is fire-and-forget: build the alert
            // payload as if it were a regular audit row and feed it
            // straight to whichever forwarders are subscribed to
            // `audit` log_type. We don't go through AuditLogger::log
            // because we only have access to the registry here, not
            // the channel.
            let entry = AuditEntry::platform("alert.outbox_depth_high")
                .resource("webhook_outbox")
                .detail(serde_json::json!({
                    "depth": depth,
                    "threshold": OUTBOX_ALERT_THRESHOLD,
                    "sustained_secs": dwell.as_secs(),
                }));
            forward_to_all(http, registry, &None, &entry).await;
            *last_alert_at = Some(now);
        }
    } else {
        *over_threshold_since = None;
        *last_alert_at = None;
    }

    #[derive(sqlx::FromRow)]
    struct OutboxRow {
        id: Uuid,
        forwarder_id: Uuid,
        payload: serde_json::Value,
        attempts: i32,
    }

    let due: Vec<OutboxRow> = sqlx::query_as(
        "SELECT id, forwarder_id, payload, attempts \
           FROM webhook_outbox \
          WHERE next_attempt_at <= now() \
          ORDER BY next_attempt_at ASC \
          LIMIT 100",
    )
    .fetch_all(db)
    .await?;

    if due.is_empty() {
        return Ok(());
    }

    let registry_guard = registry.read().await;
    for row in due {
        // Forwarder may have been deleted (cascade should have removed
        // the row, but races happen) or disabled — skip and let the
        // next tick reconsider. Disabled forwarders also stop draining.
        let runtime = match registry_guard.get(&row.forwarder_id) {
            Some(rt) if rt.config.enabled => rt,
            _ => {
                continue;
            }
        };

        // Re-deserialise the entry. A schema drift between insert and
        // drain (extremely unlikely) would surface here as a parse
        // error; in that case we drop the row to avoid a poison pill.
        let entry: AuditEntry = match serde_json::from_value(row.payload.clone()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    outbox_id = %row.id,
                    error = %e,
                    "outbox payload no longer parses; dropping"
                );
                let _ = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                    .bind(row.id)
                    .execute(db)
                    .await;
                continue;
            }
        };

        match send_webhook(http, &runtime.config, &entry).await {
            Ok(()) => {
                let _ = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                    .bind(row.id)
                    .execute(db)
                    .await;
                let _ = sqlx::query(
                    "UPDATE log_forwarders SET sent_count = sent_count + 1, \
                                                last_sent_at = now(), \
                                                updated_at = now() \
                     WHERE id = $1",
                )
                .bind(row.forwarder_id)
                .execute(db)
                .await;
            }
            Err(err_msg) => {
                let next_attempts = row.attempts + 1;
                if next_attempts >= MAX_OUTBOX_ATTEMPTS {
                    // Give up — drop the row and surface a metric so
                    // the operator can investigate without an
                    // ever-growing table.
                    metrics::counter!("audit_log_dropped_total", "kind" => "webhook_outbox_exhausted")
                        .increment(1);
                    let _ = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                        .bind(row.id)
                        .execute(db)
                        .await;
                    tracing::error!(
                        forwarder_id = %row.forwarder_id,
                        attempts = next_attempts,
                        error = %err_msg,
                        "webhook outbox row exhausted; dropping"
                    );
                } else {
                    // Exponential backoff capped at 1h — see `outbox_backoff_secs`.
                    let delay_secs = outbox_backoff_secs(next_attempts);
                    let _ = sqlx::query(
                        "UPDATE webhook_outbox \
                            SET attempts = $2, \
                                last_error = $3, \
                                next_attempt_at = now() + ($4 || ' seconds')::interval \
                          WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(next_attempts)
                    .bind(&err_msg)
                    .bind(delay_secs.to_string())
                    .execute(db)
                    .await;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Forwarder implementations
// ---------------------------------------------------------------------------

/// Build an RFC 5424 syslog message from a forwarder config and audit entry.
/// Shared between UDP and TCP transports to avoid duplicating the message
/// construction logic.
fn build_syslog_message(facility: u8, entry: &AuditEntry, newline: bool) -> String {
    let severity = 6u8; // informational
    let priority = facility * 8 + severity;

    let structured_data = format!(
        "[audit@0 user_id=\"{}\" action=\"{}\" resource=\"{}\" ip=\"{}\"]",
        entry.user_id.as_deref().unwrap_or("-"),
        entry.action,
        entry.resource.as_deref().unwrap_or("-"),
        entry.ip_address.as_deref().unwrap_or("-"),
    );
    let resource = entry.resource.as_deref().unwrap_or("-");

    let mut message = format!(
        "<{priority}>1 {ts} think-watch audit - {action} {sd} {action} on {resource}",
        ts = &entry.created_at,
        action = entry.action,
        sd = structured_data,
    );
    if newline {
        message.push('\n');
    }
    message
}

fn parse_syslog_facility(config: &serde_json::Value) -> u8 {
    config
        .get("facility")
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(16) // default: local0
}

fn send_udp_syslog(runtime: &ForwarderRuntime, entry: &AuditEntry) -> Result<(), String> {
    let addr = runtime
        .config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in udp_syslog config")?;
    let facility = parse_syslog_facility(&runtime.config.config);
    let socket = runtime
        .udp_socket
        .as_ref()
        .ok_or("UDP socket not initialized")?;
    let message = build_syslog_message(facility, entry, false);

    socket
        .send_to(message.as_bytes(), addr)
        .map(|_| ())
        .map_err(|e| format!("Syslog UDP send failed: {e}"))
}

async fn send_tcp_syslog(runtime: &ForwarderRuntime, entry: &AuditEntry) -> Result<(), String> {
    let addr = runtime
        .config
        .config
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'address' in tcp_syslog config")?;
    let facility = parse_syslog_facility(&runtime.config.config);
    let message = build_syslog_message(facility, entry, true);

    let mut guard = runtime.tcp_stream.lock().await;

    // Try writing to existing stream first
    if let Some(stream) = guard.as_mut() {
        match stream.write_all(message.as_bytes()).await {
            Ok(()) => return Ok(()),
            Err(_) => {
                // Connection is stale, drop it and reconnect below
                *guard = None;
            }
        }
    }

    // Connect (or reconnect after a failed write)
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("TCP syslog connect failed: {e}"))?;
    stream
        .write_all(message.as_bytes())
        .await
        .map_err(|e| format!("TCP syslog write failed: {e}"))?;
    *guard = Some(stream);
    Ok(())
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
    // `Bytes` is cheaply cloneable (ref-counted) so the retry loop
    // doesn't deep-copy the payload on each attempt.
    let body: bytes::Bytes = serde_json::to_vec(entry)
        .map_err(|e| format!("JSON serialise: {e}"))?
        .into();

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
    hex::encode(tag)
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
                user_email: entry.user_email,
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
            user_email: entry.user_email,
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
            user_email: entry.user_email,
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
/// Run the ClickHouse schema bootstrap (initdb.d/*.sql) exactly once
/// at startup.
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

    // Schema bootstrap. Mirrors the docker entrypoint mount of
    // deploy/clickhouse/initdb.d/, embedded at compile time so the
    // binary can re-bootstrap on startup when the ClickHouse data
    // dir already exists (in which case the entrypoint init scripts
    // don't run).
    let init_sql = include_str!("../../../deploy/clickhouse/initdb.d/01_init.sql");

    // Strip `--` line comments before splitting on `;`, otherwise a
    // semicolon inside a comment (e.g. "originating handler's
    // middleware;") splits a CREATE TABLE in half. The init file
    // contains no string literals with `--`, so naive line-prefix
    // stripping is safe.
    let cleaned: String = init_sql
        .lines()
        .map(|l| l.split_once("--").map(|(code, _)| code).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");

    for statement in cleaned.split(';') {
        let stmt = statement.trim();
        if stmt.is_empty() {
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
