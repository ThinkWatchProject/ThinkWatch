//! Core types of the audit pipeline: the log-type enum, the body-
//! capture status enum, the [`AuditEntry`] DTO + its builder, and the
//! per-table ClickHouse row structs.
//!
//! Pure data layer — no I/O. Other modules in `audit/` add behavior
//! around these types (actors construct entries, logger ships them,
//! clickhouse flushes them, …).

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::sanitize::sanitize_detail;

// ---------------------------------------------------------------------------
// Log types — each maps to a distinct ClickHouse table
// ---------------------------------------------------------------------------

/// The category of a log entry, determining which ClickHouse table it's stored in
/// and which forwarders will receive it.
///
/// `Audit` is the catch-all for every actor-attributed event — API key usage,
/// user / team / provider / role / settings mutations, login attempts. There
/// used to be a separate `Platform` variant for management operations, but
/// the schemas were identical (with `audit_logs` strictly richer: it carries
/// `api_key_id` + `trace_id`), so the split only fragmented the explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogType {
    /// HTTP access log (both gateway & console)
    Access,
    /// Runtime application logs (info/warn/error/debug)
    App,
    /// Every actor-attributed event: API key usage AND platform management.
    Audit,
    /// Gateway request logs (model calls, tokens, costs)
    Gateway,
    /// MCP tool invocation logs
    Mcp,
}

impl LogType {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogType::Access => "access",
            LogType::App => "app",
            LogType::Audit => "audit",
            LogType::Gateway => "gateway",
            LogType::Mcp => "mcp",
        }
    }

    pub fn index_id(&self) -> &'static str {
        match self {
            LogType::Access => "access_logs",
            LogType::App => "app_logs",
            LogType::Audit => "audit_logs",
            LogType::Gateway => "gateway_logs",
            LogType::Mcp => "mcp_logs",
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

/// Canonical values for `gateway_logs.body_capture_status` and
/// `mcp_logs.body_capture_status`. Lives in `common` so producers
/// (gateway proxy, mcp-gateway proxy), consumers (handlers, flush
/// mappers), AND tests all reference the same source of truth — a
/// typo on one side previously could land a body row with an
/// unrecognised status that the dashboard then silently rendered
/// without a badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyCaptureStatus {
    /// Body fit inline and was captured in full.
    Captured,
    /// Body exceeded `audit.body_max_bytes` AND no offload backend
    /// was available — the cell holds the original text up to the
    /// cap with a `...` sentinel appended.
    Truncated,
    /// Either `audit.capture_request_bodies` or
    /// `audit.capture_response_bodies` (or the MCP equivalents) was
    /// off at write time. Cells are NULL.
    Disabled,
    /// The row was emitted from the response-cache shortcut — the
    /// body cell holds the cached completion, which the auditor
    /// distinguishes from a fresh capture via this status alone.
    FromCache,
    /// Body offloaded to the configured S3-compatible blob store;
    /// the cell holds an `s3://bucket/key` URL the body-viewer
    /// endpoint dereferences at read time. Distinct from
    /// `Truncated` so dashboards searching for evidence gaps can
    /// filter the offloaded set out of the lost set.
    Offloaded,
    /// Capture itself failed (serialization error, offload error
    /// past the truncation fallback, …). Cells may carry a
    /// `[blob offload failed: …]` placeholder; treat as data loss.
    Error,
}

impl BodyCaptureStatus {
    /// The string written to ClickHouse and read back by handlers.
    /// LowCardinality column, so the set is stable + small.
    pub fn as_str(&self) -> &'static str {
        match self {
            BodyCaptureStatus::Captured => "captured",
            BodyCaptureStatus::Truncated => "truncated",
            BodyCaptureStatus::Disabled => "disabled",
            BodyCaptureStatus::FromCache => "from_cache",
            BodyCaptureStatus::Offloaded => "offloaded",
            BodyCaptureStatus::Error => "error",
        }
    }
}

impl std::fmt::Display for BodyCaptureStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Stable identity for the api key across rotation. Always set
    /// to `api_keys.lineage_id` for the row that authenticated this
    /// request; `None` for standalone admin actions or
    /// session-token requests where no api key is involved. Per-key
    /// analytics group on this column instead of `api_key_id` to
    /// roll up usage across rotation generations.
    #[serde(default)]
    pub api_key_lineage_id: Option<String>,
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
    /// Client-supplied multi-turn conversation id. Rows with the same
    /// `session_id` collapse into one expandable conversation in the
    /// trace UI. Only populated on gateway logs today; other log types
    /// ignore it. Captured from the `x-session-id` request header.
    ///
    /// `#[serde(default)]` keeps backward compat with outbox entries
    /// serialised before this field existed — the drain worker pulls
    /// them back out of JSONB and deserialises without failing.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Captured request body (serialized JSON for gateway, raw JSON
    /// for MCP `tools/call` arguments). `None` when capture is
    /// disabled via `audit.capture_request_bodies` / `audit.
    /// capture_tool_arguments`, or when no body applies (admin
    /// actions). On gateway rows this lands in `gateway_logs.
    /// request_body`; on MCP rows it lands in `mcp_logs.tool_arguments`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// Captured response body (serialized completion for gateway,
    /// JSON-RPC `result` for MCP). Same gating as `request_body`.
    /// Lands in `gateway_logs.response_body` / `mcp_logs.tool_result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Persisted as a `LowCardinality(Nullable(String))` column;
    /// [`BodyCaptureStatus::as_str`] is the canonical mapping
    /// producers / consumers should use rather than typing the
    /// strings inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_capture_status: Option<String>,
    /// Original captured body size in bytes. Set explicitly by the
    /// caller so it can reflect the user's ACTUAL payload size even
    /// when the body cell was substituted for an `s3://...` URL on
    /// offload. `None` means "flush mapper should fall back to
    /// `request_body.len()`" — the right behavior for inline cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body_bytes: Option<u32>,
    /// Same as `request_body_bytes` but for the response cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_bytes: Option<u32>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Per-table Row structs for ClickHouse SDK insert
// ---------------------------------------------------------------------------

/// audit_logs — API key usage audit
#[derive(Debug, clickhouse::Row, Serialize)]
pub(super) struct ChAuditRow {
    pub(super) id: String,
    pub(super) user_id: Option<String>,
    pub(super) user_email: Option<String>,
    pub(super) api_key_id: Option<String>,
    // Stable identity across api-key rotation. Sits immediately
    // after `api_key_id` to match the column order in
    // `deploy/clickhouse/initdb.d/01_init.sql` — the clickhouse
    // crate's Row derive binds positionally, not by name.
    pub(super) api_key_lineage_id: Option<String>,
    pub(super) action: String,
    pub(super) resource: Option<String>,
    pub(super) resource_id: Option<String>,
    pub(super) detail: Option<String>,
    pub(super) ip_address: Option<String>,
    pub(super) user_agent: Option<String>,
    // Order matches CREATE TABLE in deploy/clickhouse/initdb.d/01_init.sql; trace_id
    // must sit here (between user_agent and created_at) so CH's columnar
    // insert lines up. If you move one, move both.
    pub(super) trace_id: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub(super) created_at: chrono::DateTime<Utc>,
}

/// gateway_logs — model request logs. `user_email` is a point-in-time
/// snapshot of the user's email — queries against historical rows stay
/// readable even after the user is hard-deleted.
#[derive(Debug, clickhouse::Row, Serialize)]
pub(super) struct ChGatewayRow {
    pub(super) id: String,
    pub(super) user_id: Option<String>,
    pub(super) user_email: Option<String>,
    pub(super) api_key_id: Option<String>,
    // Stable identity across api-key rotation. Same position as in
    // `deploy/clickhouse/initdb.d/01_init.sql::gateway_logs`.
    pub(super) api_key_lineage_id: Option<String>,
    pub(super) model_id: Option<String>,
    pub(super) provider: Option<String>,
    // Upstream model actually sent to the provider — distinct from
    // `model_id` (the abstract id the client requested). Same column
    // ordering as `gateway_logs.upstream_model` in 01_init.sql.
    pub(super) upstream_model: Option<String>,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    // Raw ClickHouse encoding of `Decimal(18, 10)` — the value is
    // `decimal × 10^10` stored as an i64. The clickhouse 0.13 crate
    // has no Decimal type of its own, but its RowBinary serializer
    // maps i64 1:1 to a `Decimal(_, 10)` column (CH stores Decimal64
    // natively as i64). `cost_decimal::encode_i64` / `decode_i64`
    // are the only sites that should create or unwrap these raw
    // integers.
    pub(super) cost_usd: Option<i64>,
    pub(super) latency_ms: Option<i64>,
    pub(super) status_code: Option<i64>,
    pub(super) ip_address: Option<String>,
    pub(super) user_agent: Option<String>,
    pub(super) detail: Option<String>,
    pub(super) trace_id: Option<String>,
    // Match gateway_logs column order from
    // deploy/clickhouse/initdb.d/01_init.sql: session_id sits between
    // trace_id and created_at. The clickhouse crate's Row derive
    // names columns in the INSERT by struct field order, so keep
    // them aligned.
    pub(super) session_id: Option<String>,
    // Body capture columns appended after session_id by the ALTER
    // TABLE in 01_init.sql. Order MUST match the SQL `AFTER` chain:
    // request_body → response_body → request_body_bytes →
    // response_body_bytes → body_capture_status, then created_at.
    pub(super) request_body: Option<String>,
    pub(super) response_body: Option<String>,
    pub(super) request_body_bytes: Option<u32>,
    pub(super) response_body_bytes: Option<u32>,
    pub(super) body_capture_status: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub(super) created_at: chrono::DateTime<Utc>,
}

/// mcp_logs — MCP tool invocation logs. `user_email` snapshotted as
/// in ChGatewayRow.
#[derive(Debug, clickhouse::Row, Serialize)]
pub(super) struct ChMcpRow {
    pub(super) id: String,
    pub(super) user_id: Option<String>,
    pub(super) user_email: Option<String>,
    pub(super) server_id: Option<String>,
    pub(super) server_name: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) duration_ms: Option<i64>,
    pub(super) status: Option<String>,
    pub(super) error_message: Option<String>,
    pub(super) ip_address: Option<String>,
    pub(super) detail: Option<String>,
    // Body capture columns appended after `detail` by the ALTER
    // TABLE in 01_init.sql. Same ordering contract as ChGatewayRow:
    // tool_arguments → tool_result → arguments_bytes → result_bytes
    // → body_capture_status, BEFORE trace_id.
    pub(super) tool_arguments: Option<String>,
    pub(super) tool_result: Option<String>,
    pub(super) arguments_bytes: Option<u32>,
    pub(super) result_bytes: Option<u32>,
    pub(super) body_capture_status: Option<String>,
    pub(super) trace_id: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub(super) created_at: chrono::DateTime<Utc>,
}

/// app_logs — runtime tracing logs
#[derive(Debug, clickhouse::Row, Serialize)]
pub(super) struct ChAppLogRow {
    pub(super) id: String,
    pub(super) level: String,
    pub(super) target: String,
    pub(super) message: String,
    pub(super) fields: Option<String>,
    pub(super) span: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub(super) created_at: chrono::DateTime<Utc>,
}

/// access_logs — HTTP access log. `user_email` snapshotted as in ChGatewayRow.
#[derive(Debug, clickhouse::Row, Serialize)]
pub(super) struct ChAccessRow {
    pub(super) id: String,
    pub(super) method: String,
    pub(super) path: String,
    pub(super) status_code: u16,
    pub(super) latency_ms: i64,
    pub(super) port: u16,
    pub(super) user_id: Option<String>,
    pub(super) user_email: Option<String>,
    pub(super) ip_address: Option<String>,
    pub(super) user_agent: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub(super) created_at: chrono::DateTime<Utc>,
}

/// Parse RFC3339 string to DateTime<Utc>, fallback to now.
pub(super) fn parse_created_at(s: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

pub(super) fn detail_str(detail: &mut Option<serde_json::Value>) -> Option<String> {
    sanitize_detail(detail);
    detail.as_ref().map(|v| v.to_string())
}

pub(super) fn detail_field<T: serde::de::DeserializeOwned>(
    detail: &Option<serde_json::Value>,
    key: &str,
) -> Option<T> {
    detail
        .as_ref()?
        .get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Extract a Decimal-stringified field from the audit JSON detail
/// and encode it as the raw i64 ClickHouse expects under a
/// `Decimal(_, 10)` column. The producer side (proxy.rs emit_*
/// helpers) always writes cost_usd as a `Decimal::to_string()`
/// string so the f64 intermediate in the JSON number path can't
/// collapse precision.
pub(super) fn detail_cost_usd(detail: &Option<serde_json::Value>) -> Option<i64> {
    use std::str::FromStr;
    let raw = detail.as_ref()?.get("cost_usd")?;
    let text = raw.as_str()?;
    let decimal = rust_decimal::Decimal::from_str(text).ok()?;
    Some(crate::cost_decimal::encode_i64(decimal))
}

impl AuditEntry {
    /// Bare constructor — no actor attribution. Prefer an `AuditActor`
    /// impl (`AuthUser`, `AnonymousActor`, `OAuthCallbackActor`,
    /// `SystemActor`, `GatewayActor`) so the right ip / user_id /
    /// user_agent / etc. land by construction. Direct use is reserved
    /// for: (a) the actor impls themselves, (b) tests synthesizing
    /// entries to exercise the audit pipeline. The `#[deprecated]`
    /// attribute turns "I forgot to attribute this event" from a
    /// silent forensic gap into a compile-time warning — see the
    /// six review passes that chased this exact class of bug into
    /// the design of `AuditActor`.
    #[doc(hidden)]
    #[deprecated(
        note = "Use an AuditActor impl (auth_user.audit(\"…\"), SystemActor.audit(\"…\"), AnonymousActor { … }.audit(\"…\"), etc.) so actor fields are filled by construction. If you genuinely need a bare entry (actor impl body, test fixture), allow(deprecated) explicitly."
    )]
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            log_type: LogType::Audit,
            user_id: None,
            user_email: None,
            api_key_id: None,
            api_key_lineage_id: None,
            action: action.into(),
            resource: None,
            resource_id: None,
            detail: None,
            ip_address: None,
            user_agent: None,
            trace_id: None,
            session_id: None,
            request_body: None,
            response_body: None,
            body_capture_status: None,
            request_body_bytes: None,
            response_body_bytes: None,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    /// Create entry for gateway request logs.
    ///
    /// Bare constructor — same deprecation reasoning as `::new`. Use
    /// `GatewayActor.audit(action)` instead; it sets `LogType::Gateway`
    /// automatically AND pre-fills user/api_key/ip/session fields by
    /// construction, so handlers can't ship a gateway log row with
    /// missing actor attribution.
    #[doc(hidden)]
    #[deprecated(
        note = "Use GatewayActor.audit(\"…\") — sets LogType::Gateway and pre-fills actor fields."
    )]
    pub fn gateway(action: impl Into<String>) -> Self {
        #[allow(deprecated)]
        let mut entry = Self::new(action);
        entry.log_type = LogType::Gateway;
        entry
    }

    /// Create entry for MCP tool invocation logs.
    ///
    /// Bare constructor — same deprecation reasoning as `::new`. Use
    /// `McpActor.audit(action)` instead.
    #[doc(hidden)]
    #[deprecated(
        note = "Use McpActor.audit(\"…\") — sets LogType::Mcp and pre-fills actor fields."
    )]
    pub fn mcp(action: impl Into<String>) -> Self {
        #[allow(deprecated)]
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

    pub fn api_key_lineage_id(mut self, id: Uuid) -> Self {
        self.api_key_lineage_id = Some(id.to_string());
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

    /// Attach a captured request body. The string is treated as
    /// opaque: serialization, truncation, and optional PII redaction
    /// are the caller's responsibility, and whatever they hand in
    /// lands verbatim in ClickHouse.
    pub fn request_body(mut self, body: impl Into<String>) -> Self {
        self.request_body = Some(body.into());
        self
    }

    /// Attach a captured response body. Same opaque-string contract
    /// as `request_body`.
    pub fn response_body(mut self, body: impl Into<String>) -> Self {
        self.response_body = Some(body.into());
        self
    }

    /// Mark how the body fields were populated. See `AuditEntry::
    /// body_capture_status` for the canonical value list.
    pub fn body_capture_status(mut self, status: impl Into<String>) -> Self {
        self.body_capture_status = Some(status.into());
        self
    }

    /// Override the bytes reported for the request body. Use when the
    /// stored cell isn't the same size as the original payload — e.g.
    /// the body was offloaded to S3 and the cell holds a short URL but
    /// the audit row should still report the user's actual payload
    /// size. Without this the flush mapper falls back to
    /// `request_body.len()`, which is the right answer for inline.
    pub fn request_body_bytes(mut self, bytes: u32) -> Self {
        self.request_body_bytes = Some(bytes);
        self
    }

    /// Same as `request_body_bytes` for the response cell.
    pub fn response_body_bytes(mut self, bytes: u32) -> Self {
        self.response_body_bytes = Some(bytes);
        self
    }

    /// Correlate this row with other events for the same request.
    /// Typically the gateway's `metadata.request_id`. Omit for admin
    /// actions that aren't tied to a gateway call.
    pub fn trace_id(mut self, id: impl Into<String>) -> Self {
        self.trace_id = Some(id.into());
        self
    }

    /// Group multi-turn conversation rows under one id. Sourced from
    /// the `x-session-id` request header on AI gateway calls. Only
    /// `gateway_logs` stores this today; other log types drop it.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
}

#[cfg(test)]
#[allow(deprecated)] // Tests synthesize audit entries; bare ::new is fine here.
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
    fn default_entry_routes_to_audit_logs() {
        // Every actor-attributed event — API key usage AND platform
        // management — lands in `audit_logs`. There used to be a
        // separate `Platform` variant; the schemas were identical,
        // so the split was collapsed to make the explorer single-stream.
        let entry = AuditEntry::new("user.created");
        assert_eq!(entry.log_type, LogType::Audit);
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
