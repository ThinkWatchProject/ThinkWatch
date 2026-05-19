use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use rand::RngExt;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::cache::ResponseCache;
use crate::content_filter::{Action, ContentFilter};
use crate::cost_tracker::CostTracker;
use crate::health::{CircuitBreakerConfig, HealthTracker, RouteHealth};
use crate::metadata::RequestMetadata;
use crate::model_mapping::ModelMapper;
use crate::pii_redactor::{PiiRedactor, PiiStreamRestorer};
use crate::providers::traits::{ChatCompletionRequest, GatewayError};
use crate::quota::QuotaManager;
use crate::rate_limiter::RateLimiter;
use crate::router::{AffinityMode, ModelRouter, RouteEntry};
use crate::strategy::{self, RoutingStrategy};
use crate::streaming::stream_to_sse_with_restorer;
use std::str::FromStr;
use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::limits::{
    self, BudgetCap, BudgetSubject, RateLimitRule, RateLimitSubject, RateMetric, Surface,
    SurfaceConstraints, sliding, weight,
};

/// Shared application state for the gateway proxy handlers.
#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<ArcSwap<ModelRouter>>,
    pub model_mapper: Arc<ModelMapper>,
    /// Hot-swappable so admins can update rules without restarting the gateway.
    pub content_filter: Arc<ArcSwap<ContentFilter>>,
    pub quota: Arc<QuotaManager>,
    pub cache: Arc<ResponseCache>,
    /// Hot-swappable so admins can update PII patterns without restarting.
    pub pii_redactor: Arc<ArcSwap<PiiRedactor>>,
    pub cost_tracker: Arc<CostTracker>,
    pub rate_limiter: Arc<RateLimiter>,
    /// PG pool — used to query enabled rate-limit rules and budget caps
    /// per request. Cached above the proxy via `WeightCache` for the
    /// model weights; raw rules go through a separate cache later.
    pub db: PgPool,
    /// Redis client used by the bucketed sliding-window engine and the
    /// natural-period budget counters. Same connection used by `quota`,
    /// `cache`, and the rest of the gateway.
    pub redis: fred::clients::Client,
    /// LRU cache mapping `model_id → (input_weight, output_weight)`.
    /// Looked up once per request to convert raw token counts into
    /// the weighted-token cost the engine consumes.
    pub weight_cache: weight::WeightCache,
    /// Dynamic system settings — read in the hot path to honor
    /// `security.rate_limit_fail_closed` (and other future toggles)
    /// without restarting the gateway. The cache is in-process so
    /// the lookup is a `RwLock::read` + `HashMap::get`.
    pub dynamic_config: Arc<DynamicConfig>,
    /// Audit sink — used to emit one `gateway_logs` row per completed
    /// request (trace_id, tokens, latency, status). Wired up from the
    /// server so the gateway crate doesn't have to build its own.
    pub audit: think_watch_common::audit::AuditLogger,
    /// Per-route rolling-window error / latency tracker. Drives the
    /// circuit-breaker filter at selection time and the `latency`
    /// strategy's weight calculation. Backed by Redis so all gateway
    /// replicas share the same view. See `crate::health` for details.
    pub health: Arc<HealthTracker>,
    /// Body-offload store for the audit pipeline. Oversize request /
    /// response bodies get uploaded to S3-compatible storage instead
    /// of landing inline in CH; the audit row stores an `s3://...`
    /// pointer that the body-viewer endpoints dereference. Defaults
    /// to `InlineStore` (no-op) when no S3 backend is configured —
    /// see `think_watch_common::blob_store` for the policy.
    pub blob_store: Arc<dyn think_watch_common::blob_store::BlobStore>,
}

/// Identity information extracted from the auth middleware.
///
/// Carries the resolved subject IDs the proxy needs in order to
/// query the `rate_limit_rules` / `budget_caps` engine.
#[derive(Debug, Clone, Default)]
pub struct GatewayRequestIdentity {
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub api_key_id: Option<String>,
    /// Stable identity that survives api-key rotation. Carries the
    /// `api_keys.lineage_id` of the row that authenticated this
    /// request. Stamped onto every `gateway_logs` emit so the
    /// "this logical key's usage" rollup never has to recurse on
    /// PG via `rotated_from_id`.
    pub api_key_lineage_id: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    /// Merged-across-roles inline limits (most restrictive per
    /// surface+metric+window / surface+period). Computed once by the
    /// auth middleware via `rbac::compute_user_surface_constraints`
    /// and consumed directly here — no side-table lookups on the
    /// hot path.
    pub surface_constraints: SurfaceConstraints,
    /// Resolved client IP (honours `client_ip_source` + `trusted_proxies`).
    /// Populated by the API-key middleware via `extract_client_ip` so
    /// every `gateway_logs` row carries it without each handler reading
    /// headers themselves. `None` only if extraction failed.
    pub ip_address: Option<String>,
}

/// Materialize the merged surface constraints into `RateLimitRule`
/// rows keyed by the authenticated user id. Redis counters now live
/// at `ratelimit:<surface>:user:<user_id>:...` — one set per user
/// regardless of how many roles they hold. Roles merged to empty
/// (no user_id, or no rules) produce an empty list.
fn rules_for_ai_gateway(identity: &GatewayRequestIdentity) -> Vec<RateLimitRule> {
    let Some(user_id) = identity
        .user_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Vec::new();
    };
    let Some(block) = identity.surface_constraints.block(Surface::AiGateway) else {
        return Vec::new();
    };
    block
        .rules
        .iter()
        .filter(|r| r.enabled)
        .map(|r| RateLimitRule {
            // Synthetic id — stable across a single request so the
            // exceeded_index in `CheckOutcome` maps back to the same
            // rule without needing a persistence layer.
            id: Uuid::nil(),
            subject_kind: RateLimitSubject::User,
            subject_id: user_id,
            surface: Surface::AiGateway,
            metric: r.metric,
            window_secs: r.window_secs,
            max_count: r.max_count,
            enabled: true,
            // In-memory synthesis — override metadata lives on persisted rows only.
            expires_at: None,
            reason: None,
            created_by: None,
        })
        .collect()
}

fn budgets_for_ai_gateway(identity: &GatewayRequestIdentity) -> Vec<BudgetCap> {
    let Some(user_id) = identity
        .user_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Vec::new();
    };
    let Some(block) = identity.surface_constraints.block(Surface::AiGateway) else {
        return Vec::new();
    };
    block
        .budgets
        .iter()
        .filter(|b| b.enabled)
        .map(|b| BudgetCap {
            id: Uuid::nil(),
            subject_kind: BudgetSubject::User,
            subject_id: user_id,
            period: b.period,
            limit_tokens: b.limit_tokens,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        })
        .collect()
}

/// Post-flight accounting for one completed AI gateway request.
///
/// Runs the token-metric sliding rules and budget caps against the
/// real prompt/completion token counts the upstream returned. Used
/// from BOTH the non-streaming branch (called inline after the
/// upstream future resolves) and the streaming branch (called from
/// `stream_to_sse`'s `on_done` callback after the SSE stream is
/// drained).
///
/// All errors are logged and swallowed — by the time we get here
/// the caller has already received their response, so refusing to
/// account isn't an option.
///
/// Emit a `gateway_logs` row into ClickHouse via the shared audit
/// pipeline. Called exactly once per completed AI request (streaming
/// and non-streaming) so operators can run `GET /api/admin/trace/{id}`
/// and see the gateway leg of the fan-out.
///
/// `status_code` is the HTTP status we're about to return. For
/// streaming responses where the connection could still break after
/// this point we still write 200 — partial completions carry their
/// own `outcome=cancelled` counter in the streaming layer.
/// Per-handler error-logging context. Built once near the top of each
/// AI proxy handler and consumed by `return Err(ctx.emit(...))` at
/// early-return points so an error never leaves the gateway without a
/// matching `gateway_logs` row. The success path uses
/// `emit_gateway_log` directly because it has tokens / cost info that
/// the error path doesn't.
///
/// Fields are owned (cheap clones at request entry) because the
/// handler later mutates `request` and a borrow of `request.model`
/// would block that with a partial-borrow error. The audit handle is
/// borrowed because it's already an `Arc` internally.
struct LogCtx<'a> {
    audit: &'a think_watch_common::audit::AuditLogger,
    trace_id: String,
    session_id: Option<String>,
    user_id: Option<String>,
    user_email: Option<String>,
    api_key_id: Option<String>,
    api_key_lineage_id: Option<String>,
    ip_address: Option<String>,
    /// May be "(unknown)" when the failure happens before the model
    /// has been resolved (e.g. transform errors on malformed bodies).
    model: String,
    started: std::time::Instant,
}

impl LogCtx<'_> {
    /// Emit the error row and return the error unchanged so call sites
    /// stay one-line:  `return Err(ctx.emit(GatewayError::...));`
    fn emit(&self, err: GatewayError) -> GatewayError {
        // ctx.emit fires on pre-route-selection failures (allowed_models
        // reject, preflight rate-limit, content filter block, route
        // lookup miss). The provider and its region are only resolved
        // after a route is picked, so we legitimately pass None here —
        // the resulting gateway_logs row has a NULL provider / region,
        // which is correct: no provider handled the request.
        emit_gateway_error_log(
            self.audit,
            &self.trace_id,
            self.session_id.as_deref(),
            self.user_id.as_deref(),
            self.user_email.as_deref(),
            self.api_key_id.as_deref(),
            self.api_key_lineage_id.as_deref(),
            self.ip_address.as_deref(),
            &self.model,
            None,
            self.started.elapsed().as_millis() as i64,
            &err,
            // Pre-route-selection failures don't have a parsed request
            // we can safely serialize (transform errors, malformed JSON,
            // etc. land here). Body capture for these paths is a follow-
            // up; for now the row writes through with NULL bodies.
            BodyCapture::default(),
        );
        err
    }
}

/// Resolve the trace id for an AI request. Prefers a caller-supplied
/// `x-trace-id` header (length-bounded ASCII, no control chars) so a
/// client that wants its AI request linked with a follow-on MCP
/// tools/call can pin both legs to one id. Falls back to a UUID.
///
/// Mirrors the same validation rules as the access_log middleware and
/// the MCP transport layer — keep all three in sync if you change one.
fn resolve_trace_id(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Resolve the multi-turn conversation id for an AI request from the
/// `x-session-id` header. Unlike `trace_id`, session_id is purely
/// optional — absent means the client isn't grouping turns, and we
/// store NULL so the ClickHouse `idx_session` bloom filter stays
/// selective for rows that do carry an id.
///
/// Same validation envelope as `resolve_trace_id` so the header value
/// is always safe to round-trip through log fields.
fn resolve_session_id(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()))
}

/// Thin wrapper kept for call-site readability; delegates to
/// `GatewayError::status_code` so the error-path `gateway_logs` row,
/// the streaming `StreamOutcome::UpstreamError` log row, and
/// `GatewayErrorResponse::into_response` all share one mapping.
fn gateway_error_status(err: &GatewayError) -> i64 {
    err.status_code()
}

// ---------------------------------------------------------------------------
// Body capture
// ---------------------------------------------------------------------------
//
// Full request/response payload snapshots for the enterprise audit
// trail. Gating + truncation + optional PII redaction happens once
// per request inside `prepare_body_capture`; the resulting struct is
// passed verbatim into every `emit_gateway_log*` call site so the
// success / streaming / error / cache-hit paths all carry the same
// payload semantics.

/// Body capture status values written into the
/// `gateway_logs.body_capture_status` column. Kept as `&'static str`
/// constants so a typo can't desync the producer side from the
/// audit-query side.
const BODY_CAPTURED: &str = "captured";
const BODY_TRUNCATED: &str = "truncated";
const BODY_DISABLED: &str = "disabled";
const BODY_FROM_CACHE: &str = "from_cache";
/// One body (or both) exceeded the inline cap and was uploaded to the
/// configured S3-compatible blob store. The corresponding body column
/// holds an `s3://bucket/key` pointer instead of the raw payload; the
/// body-viewer endpoint dereferences before returning it to the
/// auditor. Distinct from "truncated" so an operator searching for
/// audit gaps doesn't see oversize-but-preserved bodies grouped with
/// the truly-lost truncated ones.
const BODY_OFFLOADED: &str = "offloaded";

/// Captured payload snapshot. Cheap to construct + clone — the
/// strings are already truncated / redacted / serialized by
/// `prepare_body_capture` so the per-call-site cost is just two
/// `Option<String>` clones.
///
/// `request_bytes` / `response_bytes` carry the ORIGINAL body size,
/// not the cell size — when a body offloads to S3 the cell holds a
/// short `s3://...` URL while the byte count needs to reflect the
/// user's actual payload (auditors aggregating "total prompt bytes
/// captured this month" would otherwise see ~80 bytes per offloaded
/// row and a wildly wrong number). The flush mapper reads these in
/// preference to `cell.len()` exactly to break that asymmetry.
#[derive(Default, Clone)]
struct BodyCapture {
    request: Option<String>,
    response: Option<String>,
    request_bytes: Option<u32>,
    response_bytes: Option<u32>,
    status: Option<&'static str>,
}

impl BodyCapture {
    fn disabled() -> Self {
        Self {
            request: None,
            response: None,
            request_bytes: None,
            response_bytes: None,
            status: Some(BODY_DISABLED),
        }
    }

    /// Attach the captured strings + status to an `AuditEntry`. No-op
    /// when all three fields are empty.
    fn apply(
        self,
        mut entry: think_watch_common::audit::AuditEntry,
    ) -> think_watch_common::audit::AuditEntry {
        if let Some(r) = self.request {
            entry = entry.request_body(r);
        }
        if let Some(r) = self.response {
            entry = entry.response_body(r);
        }
        if let Some(b) = self.request_bytes {
            entry = entry.request_body_bytes(b);
        }
        if let Some(b) = self.response_bytes {
            entry = entry.response_body_bytes(b);
        }
        if let Some(s) = self.status {
            entry = entry.body_capture_status(s);
        }
        entry
    }
}

/// Walk a request body + optional response through the
/// dynamic-config-driven capture pipeline:
///   1. capture-enabled gate (per-field)
///   2. optional PII redaction (when `audit.body_redact_pii` is on)
///   3. blob-store offload when oversize and a backend is configured
///   4. byte-cap truncation when offload isn't available (fallback)
///
/// `messages` is the post-PII-redaction set the gateway already
/// passes to upstream; for the audit blob we want the version users
/// actually authored. The caller hands us the original
/// pre-redaction slice when both forms exist (`prepare_body_capture`
/// itself does not know which was sent upstream).
///
/// `trace_id` is woven into the offload object key so an operator
/// browsing the bucket can correlate objects back to the audit row
/// without a CH query.
async fn prepare_body_capture(
    dynamic_config: &DynamicConfig,
    pii_redactor: &PiiRedactor,
    blob_store: &std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    trace_id: &str,
    messages: &[crate::providers::traits::ChatMessage],
    response: Option<&crate::providers::traits::ChatCompletionResponse>,
) -> BodyCapture {
    let capture_req = dynamic_config.audit_capture_request_bodies().await;
    let capture_resp = dynamic_config.audit_capture_response_bodies().await;
    if !capture_req && !capture_resp {
        return BodyCapture::disabled();
    }
    let max_bytes = dynamic_config.audit_body_max_bytes().await as usize;
    let redact_pii = dynamic_config.audit_body_redact_pii().await;
    let can_offload = blob_store.can_offload();

    let mut truncated_flag = false;
    let mut offloaded_flag = false;
    let mut request_bytes: Option<u32> = None;
    let mut response_bytes: Option<u32> = None;
    let request = if capture_req {
        let raw =
            serde_json::to_string(messages).unwrap_or_else(|_| "[serialize_error]".to_owned());
        request_bytes = Some(raw.len() as u32);
        Some(
            process_body(
                raw,
                max_bytes,
                redact_pii,
                pii_redactor,
                blob_store,
                can_offload,
                trace_id,
                "request",
                &mut truncated_flag,
                &mut offloaded_flag,
            )
            .await,
        )
    } else {
        None
    };
    let response_body = match (capture_resp, response) {
        (true, Some(resp)) => {
            let raw =
                serde_json::to_string(resp).unwrap_or_else(|_| "[serialize_error]".to_owned());
            response_bytes = Some(raw.len() as u32);
            Some(
                process_body(
                    raw,
                    max_bytes,
                    redact_pii,
                    pii_redactor,
                    blob_store,
                    can_offload,
                    trace_id,
                    "response",
                    &mut truncated_flag,
                    &mut offloaded_flag,
                )
                .await,
            )
        }
        _ => None,
    };
    let status = if request.is_none() && response_body.is_none() {
        BODY_DISABLED
    } else if offloaded_flag {
        // Offload + truncation are mutually exclusive per field, but a
        // single emit can carry one offloaded body and one truncated
        // body if e.g. request fit inline and response was huge. Pick
        // `offloaded` as the dominant status because it's the more
        // informative one — truncation would lose data, offload didn't.
        BODY_OFFLOADED
    } else if truncated_flag {
        BODY_TRUNCATED
    } else {
        BODY_CAPTURED
    };
    BodyCapture {
        request,
        response: response_body,
        request_bytes,
        response_bytes,
        status: Some(status),
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_body(
    mut s: String,
    max_bytes: usize,
    redact_pii: bool,
    pii_redactor: &PiiRedactor,
    blob_store: &std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    can_offload: bool,
    trace_id: &str,
    field: &'static str,
    truncated_flag: &mut bool,
    offloaded_flag: &mut bool,
) -> String {
    if redact_pii {
        s = pii_redactor.redact_blob(&s);
    }
    if s.len() <= max_bytes {
        return s;
    }
    // Oversize. If a blob store is wired in, offload — auditors keep
    // the full payload at the cost of one S3 round-trip. If not, fall
    // back to char-boundary-safe truncation so we still record SOMETHING
    // (a `truncated` row beats a NULL one for investigations).
    if can_offload {
        use think_watch_common::blob_store::{BlobDecision, BlobKeyHint};
        let hint = BlobKeyHint {
            table: "gateway_logs",
            log_id: trace_id,
            field,
        };
        match blob_store
            .store_if_oversize(hint, std::mem::take(&mut s), max_bytes)
            .await
        {
            Ok(BlobDecision::Offloaded { url, .. }) => {
                *offloaded_flag = true;
                return url;
            }
            Ok(BlobDecision::Inline(returned)) => {
                // Shouldn't happen — store_if_oversize was called with
                // a string already > max_bytes — but if a future store
                // impl changes its mind, fall through to truncation.
                s = returned;
            }
            Err(e) => {
                tracing::warn!(
                    field,
                    trace_id,
                    error = %e,
                    "blob offload failed; falling back to truncation"
                );
                metrics::counter!(
                    "audit_body_offload_failed_total",
                    "field" => field.to_string()
                )
                .increment(1);
                // `s` was moved into store_if_oversize via take; we
                // only get a fresh string on Inline/Err inside the
                // closure. The Err arm above gives us nothing, so
                // re-serialize from scratch — at worst we lose this
                // body to truncation, which is the same outcome as
                // having no store configured at all.
                s = format!("[blob offload failed: {e}]");
            }
        }
    }
    *truncated_flag = true;
    // Operator-visible signal: any truncation is data loss, and a spike
    // typically means S3 is unreachable (offload silently fell back to
    // this path). Distinct counter from `audit_body_offload_failed_total`
    // because deployments without S3 configured fall through here on
    // every oversize body BY DESIGN — alerting on those would be noise.
    // Pair with `audit_body_offload_failed_total` in dashboards to
    // distinguish "S3 outage" (failed + truncated both rise) from
    // "no S3 configured, normal" (only truncated rises).
    metrics::counter!(
        "audit_body_truncated_total",
        "field" => field.to_string(),
    )
    .increment(1);
    // Leave room for the ellipsis sentinel and walk back to the
    // nearest UTF-8 char boundary — provider names / model tokens /
    // user prompts routinely include non-ASCII (CJK, emoji), and a
    // naive byte slice would panic mid-codepoint.
    let budget = max_bytes.saturating_sub(3);
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}

/// Emit a single `gateway_logs` row for a failed request. The detail
/// blob mirrors the success-path shape but also carries `error_type`
/// and `error_message` so operators can drill down without joining
/// against a separate error-log stream.
#[allow(clippy::too_many_arguments)]
fn emit_gateway_error_log(
    audit: &think_watch_common::audit::AuditLogger,
    trace_id: &str,
    session_id: Option<&str>,
    user_id: Option<&str>,
    user_email: Option<&str>,
    api_key_id: Option<&str>,
    api_key_lineage_id: Option<&str>,
    ip_address: Option<&str>,
    model_id: &str,
    provider: Option<&str>,
    latency_ms: i64,
    err: &GatewayError,
    bodies: BodyCapture,
) {
    let status = gateway_error_status(err);
    let detail = serde_json::json!({
        "model_id": model_id,
        "provider": provider,
        "input_tokens": 0i64,
        "output_tokens": 0i64,
        // Decimal-as-string in the audit JSON so the CH flush reader
        // can reconstruct exact precision — the JSON `number` path
        // would collapse through f64 in between.
        "cost_usd": Decimal::ZERO.to_string(),
        "latency_ms": latency_ms,
        "status_code": status,
        "error_type": format!("{err:?}").split('(').next().unwrap_or("Error"),
        "error_message": err.to_string(),
    });
    // Same `chat.completion` action as the success path — flush_gateway
    // drops the action when it writes ChGatewayRow, so the trace
    // endpoint distinguishes errors via `status_code` (>= 400) instead.
    // Detail carries error_type + error_message for the drill-down.
    use think_watch_common::audit::{AuditActor, GatewayActor};
    let actor = GatewayActor {
        user_id,
        user_email,
        api_key_id,
        api_key_lineage_id,
        ip: ip_address,
        session_id,
    };
    let entry = actor
        .audit("chat.completion")
        .trace_id(trace_id.to_string())
        .detail(detail);
    audit.log(bodies.apply(entry));
}

/// Same as `emit_gateway_log` but with an optional `extra` JSON object
/// whose fields are merged into the audit detail. Used by the
/// streaming on_done path to attach `error_type` / `error_message` /
/// `stream_outcome` for non-natural completions, see OBS-02.
#[allow(clippy::too_many_arguments)]
fn emit_gateway_log_with_extra(
    audit: &think_watch_common::audit::AuditLogger,
    trace_id: &str,
    session_id: Option<&str>,
    user_id: Option<&str>,
    user_email: Option<&str>,
    api_key_id: Option<&str>,
    api_key_lineage_id: Option<&str>,
    ip_address: Option<&str>,
    model_id: &str,
    provider: Option<&str>,
    upstream_model: Option<&str>,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost_usd: Decimal,
    latency_ms: i64,
    status_code: i64,
    extra: Option<serde_json::Value>,
    bodies: BodyCapture,
) {
    let mut detail = serde_json::json!({
        "model_id": model_id,
        "provider": provider,
        "upstream_model": upstream_model,
        "input_tokens": prompt_tokens as i64,
        "output_tokens": completion_tokens as i64,
        "cost_usd": cost_usd.to_string(),
        "latency_ms": latency_ms,
        "status_code": status_code,
    });
    if let (Some(serde_json::Value::Object(extra_map)), serde_json::Value::Object(detail_map)) =
        (extra, &mut detail)
    {
        for (k, v) in extra_map {
            detail_map.insert(k, v);
        }
    }
    use think_watch_common::audit::{AuditActor, GatewayActor};
    let actor = GatewayActor {
        user_id,
        user_email,
        api_key_id,
        api_key_lineage_id,
        ip: ip_address,
        session_id,
    };
    let entry = actor
        .audit("chat.completion")
        .trace_id(trace_id.to_string())
        .detail(detail);
    audit.log(bodies.apply(entry));
}

#[allow(clippy::too_many_arguments)]
fn emit_gateway_log(
    audit: &think_watch_common::audit::AuditLogger,
    trace_id: &str,
    session_id: Option<&str>,
    user_id: Option<&str>,
    user_email: Option<&str>,
    api_key_id: Option<&str>,
    api_key_lineage_id: Option<&str>,
    ip_address: Option<&str>,
    model_id: &str,
    provider: Option<&str>,
    upstream_model: Option<&str>,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost_usd: Decimal,
    latency_ms: i64,
    status_code: i64,
    bodies: BodyCapture,
) {
    use think_watch_common::audit::{AuditActor, GatewayActor};
    let actor = GatewayActor {
        user_id,
        user_email,
        api_key_id,
        api_key_lineage_id,
        ip: ip_address,
        session_id,
    };
    let entry = actor
        .audit("chat.completion")
        .trace_id(trace_id.to_string())
        .detail(serde_json::json!({
            "model_id": model_id,
            "provider": provider,
            "upstream_model": upstream_model,
            "input_tokens": prompt_tokens as i64,
            "output_tokens": completion_tokens as i64,
            "cost_usd": cost_usd.to_string(),
            "latency_ms": latency_ms,
            "status_code": status_code,
        }));
    audit.log(bodies.apply(entry));
}

/// Resolve `(prompt_tokens, completion_tokens)` from a streaming
/// result. Returns the upstream-reported usage when present; falls
/// back to a conservative estimate when the upstream didn't emit a
/// usage chunk (the OpenAI surface only does so when the client opts
/// in via `stream_options.include_usage: true`, and many clients
/// don't ask). Without this fallback, streaming requests from
/// non-include_usage clients hit `let Some(u) = result.usage else
/// { return; };` and silently bypass quota / budget / rate-limit
/// accounting — free streaming for anyone who sends `stream: true`
/// without the option.
///
/// The estimate over-approximates by design (`token_counter` already
/// over-estimates), so rate limits stay conservative. Operators can
/// distinguish exact vs estimated rows by the `stream_usage_estimated`
/// counter we bump on the fallback path.
fn stream_usage_or_estimate(
    result: &crate::streaming::StreamResult,
    request_messages: &[crate::providers::traits::ChatMessage],
) -> (u32, u32) {
    if let Some(ref u) = result.usage {
        return (u.prompt_tokens, u.completion_tokens);
    }
    metrics::counter!("gateway_stream_usage_estimated_total").increment(1);
    let prompt_tokens = crate::token_counter::count_message_tokens(request_messages);
    // `delta` is a free-form JSON Value (varies across providers);
    // pull the canonical `content` string when present and skip
    // anything else (tool_calls, refusal, vendor extensions).
    let mut completion_text = String::new();
    for chunk in &result.chunks {
        for choice in &chunk.choices {
            if let Some(content) = choice.delta.get("content").and_then(|v| v.as_str()) {
                completion_text.push_str(content);
            }
        }
    }
    let completion_tokens = crate::token_counter::estimate_tokens(&completion_text);
    (prompt_tokens, completion_tokens)
}

#[allow(clippy::too_many_arguments)]
async fn post_flight_account(
    db: sqlx::PgPool,
    redis: fred::clients::Client,
    _dynamic_config: Arc<DynamicConfig>,
    weight_cache: weight::WeightCache,
    model: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    request_rules: Vec<limits::RateLimitRule>,
    budget_caps: Vec<BudgetCap>,
    // Actor attribution for `budget.threshold_crossed` audit entries.
    // Without these the crossing log carries only `cap_id`, and the
    // cap's `subject_id` may be a team/role — operators investigating
    // a 100 %-cross had to time-join against gateway_logs to find the
    // user who pushed it over. Cloned in the streaming path (the
    // tokio task moves owned values) and inlined in the non-streaming
    // path; passing `Option<String>` keeps both call shapes flat.
    actor_user_id: Option<String>,
    actor_user_email: Option<String>,
    actor_api_key_id: Option<String>,
    actor_ip_address: Option<String>,
    audit: think_watch_common::audit::AuditLogger,
) {
    let mult = weight_cache.get(&db, &model).await;
    let weighted = weight::weighted_tokens(prompt_tokens as i64, completion_tokens as i64, mult);
    if weighted <= 0 {
        return;
    }

    // Token-metric sliding rules — same rule set the pre-flight
    // loaded, filtered to tokens. Post-flight always runs fail-open
    // because the response has already been delivered: refusing to
    // record the spend would just hide it from analytics without
    // recovering anything.
    let resolved_token_rules = sliding::resolve_rules(&request_rules, RateMetric::Tokens);
    if !resolved_token_rules.is_empty()
        && let Err(e) =
            sliding::check_and_record(&redis, &resolved_token_rules, weighted, true).await
    {
        tracing::warn!("token rate-limit accounting failed: {e}");
    }

    // Natural-period budget caps derived from the user's merged
    // role-inline constraints. `db` is unused here — kept on the
    // signature so future per-user overrides can fold in cleanly.
    let _ = db;
    if !budget_caps.is_empty() {
        let caps = budget_caps;
        {
            match limits::budget::add_weighted_tokens(&redis, &caps, weighted).await {
                Ok((_statuses, crossings)) if !crossings.is_empty() => {
                    // Emit one `budget.threshold_crossed` audit-log
                    // entry per crossing. The action is namespaced
                    // under `budget.*` so webhook forwarders can
                    // subscribe cleanly to the LogType::Audit stream
                    // and filter by action prefix. Crosses fire at
                    // 50 / 80 / 95 / 100 % (see ALERT_THRESHOLDS_PCT
                    // in common::limits::budget); webhook delivery
                    // rides the existing forwarder pipeline.
                    use think_watch_common::audit::{AuditActor, GatewayActor};
                    let actor = GatewayActor {
                        user_id: actor_user_id.as_deref(),
                        user_email: actor_user_email.as_deref(),
                        api_key_id: actor_api_key_id.as_deref(),
                        api_key_lineage_id: None,
                        ip: actor_ip_address.as_deref(),
                        session_id: None,
                    };
                    for crossing in &crossings {
                        // `.log_type(LogType::Audit)` overrides
                        // `GatewayActor`'s default LogType::Gateway —
                        // budget.threshold_crossed lands in the audit
                        // log (forwarders subscribe to it), not the
                        // gateway log table.
                        audit.log(
                            actor
                                .audit("budget.threshold_crossed")
                                .log_type(think_watch_common::audit::LogType::Audit)
                                .resource(format!("budget_cap:{}", crossing.cap_id))
                                .detail(
                                    serde_json::to_value(crossing)
                                        .unwrap_or(serde_json::Value::Null),
                                ),
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("budget add_weighted_tokens failed: {e}");
                }
            }
        }
    }
}

/// Run the requests-metric pre-flight against the limits engine for
/// one resolved request. Shared by all three AI gateway surfaces:
/// chat completions, Anthropic Messages, and the OpenAI Responses
/// API. Returns the loaded rule set so the caller can hand it to
/// `post_flight_account` after the upstream responds — saves a
/// second DB round-trip.
///
/// Honors `security.rate_limit_fail_closed`: when set, a Redis
/// outage on the engine returns `Err(LocalRateLimited)` so the
/// caller can return 429 instead of letting the request slip
/// through.
async fn preflight_request_limits(
    state: &GatewayState,
    identity: &GatewayRequestIdentity,
    model: &str,
) -> Result<(Option<Uuid>, Vec<limits::RateLimitRule>), GatewayErrorResponse> {
    let _provider_id = state.router.load().provider_id_for(model);
    // Role-inline constraints came in with the identity (materialized
    // once by the auth middleware). No DB fetch here — fail-closed is
    // only relevant for the Redis call below.
    let request_rules = rules_for_ai_gateway(identity);
    let resolved_request_rules = sliding::resolve_rules(&request_rules, RateMetric::Requests);
    if !resolved_request_rules.is_empty() {
        let fail_closed = state.dynamic_config.rate_limit_fail_closed().await;
        let outcome =
            match sliding::check_and_record(&state.redis, &resolved_request_rules, 1, !fail_closed)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    if fail_closed {
                        tracing::warn!("rate-limit redis error: {e}; failing closed");
                        return Err(GatewayError::LocalRateLimited(
                            "rate_limiter_unavailable".to_string(),
                        )
                        .into());
                    }
                    tracing::warn!("rate-limit redis error: {e}; allowing request");
                    sliding::CheckOutcome {
                        allowed: true,
                        exceeded_index: -1,
                        currents: Vec::new(),
                    }
                }
            };
        if !outcome.allowed {
            let label = (outcome.exceeded_index >= 0)
                .then(|| {
                    request_rules
                        .iter()
                        .filter(|r| r.metric == RateMetric::Requests)
                        .nth(outcome.exceeded_index as usize)
                        .map(rule_label)
                })
                .flatten()
                .unwrap_or_else(|| "rate limit".to_string());
            metrics::counter!("gateway_rate_limited_total", "metric" => "requests").increment(1);
            return Err(GatewayError::LocalRateLimited(label).into());
        }
    }
    Ok((_provider_id, request_rules))
}

// ---------------------------------------------------------------------------
// Multi-route selection: strategy-driven weights + circuit-breaker filter
// + per-model session affinity (none / provider / route).
// ---------------------------------------------------------------------------

/// What the affinity layer can pin a session to.
#[derive(Debug, Clone, Copy)]
enum AffinityHit {
    Provider(Uuid),
    Route(Uuid),
}

/// Read the affinity key for `(model, mode, user)` and translate it
/// to whichever id the mode pins on. `None` mode short-circuits.
/// We include the mode in the cache key so flipping the mode at
/// runtime invalidates stale entries automatically.
async fn check_affinity(
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
    mode: AffinityMode,
    entries: &[&RouteEntry],
) -> Option<AffinityHit> {
    use fred::interfaces::KeysInterface;
    if matches!(mode, AffinityMode::None) {
        return None;
    }
    let uid = user_id?;
    let key = format!("affinity:{}:{uid}:{model}", mode.as_str());
    let val: Option<String> = redis.get(&key).await.ok().flatten();
    let id = val.and_then(|s| Uuid::parse_str(&s).ok())?;
    match mode {
        AffinityMode::Provider => entries
            .iter()
            .any(|e| e.provider_id == id)
            .then_some(AffinityHit::Provider(id)),
        AffinityMode::Route => entries
            .iter()
            .any(|e| e.route_id == id)
            .then_some(AffinityHit::Route(id)),
        AffinityMode::None => None,
    }
}

/// Stamp a successful completion's affinity. TTL 0 disables affinity
/// entirely (a runtime kill switch via `gateway.default_affinity_ttl_secs`
/// or the per-model override).
async fn set_affinity(
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
    mode: AffinityMode,
    entry: &RouteEntry,
    ttl_secs: u32,
) {
    use fred::interfaces::KeysInterface;
    if matches!(mode, AffinityMode::None) || ttl_secs == 0 {
        return;
    }
    let Some(uid) = user_id else { return };
    let id = match mode {
        AffinityMode::Provider => entry.provider_id,
        AffinityMode::Route => entry.route_id,
        AffinityMode::None => return,
    };
    let key = format!("affinity:{}:{uid}:{model}", mode.as_str());
    let _: Result<(), _> = redis
        .set::<(), _, _>(
            &key,
            id.to_string(),
            Some(fred::types::Expiration::EX(ttl_secs as i64)),
            None,
            false,
        )
        .await;
}

/// Resolve `(strategy, affinity_mode, affinity_ttl)` for a model:
/// per-model override falls through to gateway-wide defaults.
async fn resolve_routing_config(
    state: &GatewayState,
    model: &str,
) -> (RoutingStrategy, AffinityMode, u32) {
    let model_cfg = state.router.load().config_for(model);
    let strategy = match model_cfg.strategy {
        Some(s) => s,
        None => RoutingStrategy::from_str(&state.dynamic_config.default_routing_strategy().await)
            .unwrap_or_default(),
    };
    let mode = match model_cfg.affinity_mode {
        Some(m) => m,
        None => AffinityMode::parse_or_default(&state.dynamic_config.default_affinity_mode().await),
    };
    let ttl = match model_cfg.affinity_ttl_secs {
        Some(t) => t,
        None => state.dynamic_config.default_affinity_ttl_secs().await as u32,
    };
    (strategy, mode, ttl)
}

async fn resolve_breaker_config(state: &GatewayState) -> CircuitBreakerConfig {
    CircuitBreakerConfig {
        enabled: state.dynamic_config.cb_enabled().await,
        error_pct: state.dynamic_config.cb_error_pct().await,
        min_samples: state.dynamic_config.cb_min_samples().await,
        window_secs: state.dynamic_config.cb_window_secs().await,
        open_secs: state.dynamic_config.cb_open_secs().await,
    }
}

/// Strategy/affinity/breaker context resolved once per request and
/// reused through the failover loop.
pub(crate) struct SelectionCtx<'a> {
    model_id: &'a str,
    user_id: Option<&'a str>,
    strategy: RoutingStrategy,
    affinity_mode: AffinityMode,
    affinity_ttl_secs: u32,
    latency_k: f64,
    breaker: CircuitBreakerConfig,
    state: &'a GatewayState,
}

pub(crate) async fn build_selection_ctx<'a>(
    state: &'a GatewayState,
    model_id: &'a str,
    user_id: Option<&'a str>,
) -> SelectionCtx<'a> {
    let (strategy, affinity_mode, affinity_ttl_secs) =
        resolve_routing_config(state, model_id).await;
    let latency_k = state.dynamic_config.latency_strategy_k().await;
    let breaker = resolve_breaker_config(state).await;
    SelectionCtx {
        model_id,
        user_id,
        strategy,
        affinity_mode,
        affinity_ttl_secs,
        latency_k,
        breaker,
        state,
    }
}

/// One selection attempt over the candidate set: snapshot health,
/// drop circuit-broken / already-tried candidates, compute strategy
/// weights, pick.
async fn pick_with_strategy<'a>(
    group: &[&'a RouteEntry],
    tried: &[Uuid],
    ctx: &SelectionCtx<'_>,
) -> Option<&'a RouteEntry> {
    if group.is_empty() {
        return None;
    }
    let mut healths: Vec<RouteHealth> = Vec::with_capacity(group.len());
    for entry in group {
        let h = ctx
            .state
            .health
            .snapshot(entry.route_id, ctx.breaker.window_secs)
            .await;
        healths.push(h);
    }

    // Affinity check before breaker filter — a stale affinity to a
    // now-broken route degrades cleanly: we ignore the affinity in
    // that case and let the strategy pick fresh.
    let affinity = check_affinity(
        &ctx.state.redis,
        ctx.user_id,
        ctx.model_id,
        ctx.affinity_mode,
        group,
    )
    .await;

    let mut signals: Vec<strategy::RouteSignal> = Vec::with_capacity(group.len());
    let mut excluded: Vec<bool> = Vec::with_capacity(group.len());
    for (i, entry) in group.iter().enumerate() {
        let h = &healths[i];
        let excl = !h.state.allows_selection() || tried.contains(&entry.provider_id);
        excluded.push(excl);
        let success_rate = if h.total > 0 {
            Some((1.0 - h.error_pct / 100.0).clamp(0.0, 1.0))
        } else {
            None
        };
        signals.push(strategy::RouteSignal {
            configured_weight: entry.weight,
            ewma_latency_ms: h.ewma_latency_ms,
            success_rate,
        });
    }

    let weights = strategy::compute_weights(ctx.strategy, &signals, ctx.latency_k);

    // If affinity points to a still-eligible candidate, use it.
    if let Some(hit) = affinity {
        let idx_match = group.iter().enumerate().find(|(i, e)| {
            !excluded[*i]
                && match hit {
                    AffinityHit::Provider(pid) => e.provider_id == pid,
                    AffinityHit::Route(rid) => e.route_id == rid,
                }
        });
        if let Some((_, entry)) = idx_match {
            return Some(entry);
        }
    }

    // Mask out excluded entries' weights so weighted random can't
    // pick them.
    let masked: Vec<f64> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| if excluded[i] { 0.0 } else { *w })
        .collect();

    let total: f64 = masked.iter().sum();
    let picked_idx = if total <= 0.0 {
        // No eligible candidate. Match the original `pick_weighted`
        // "all zeros" fallback: first un-excluded if any.
        (0..group.len()).find(|&i| !excluded[i])
    } else {
        let mut rng = rand::rng();
        let pick = rng.random_range(0.0..total);
        let mut acc = 0.0;
        let mut chosen = None;
        for (i, w) in masked.iter().enumerate() {
            acc += w;
            if pick < acc {
                chosen = Some(i);
                break;
            }
        }
        chosen
    };

    picked_idx.map(|i| group[i])
}

/// What the proxy handler needs back from a selection in order to
/// record health for the picked route after the request completes.
pub(crate) struct SelectionRecord {
    pub picked_route_id: Uuid,
    pub started_at: std::time::Instant,
    /// Per-attempt latency for the *picked* route in non-stream mode.
    /// `None` for streaming (the stream hasn't run when this record
    /// is built), and the caller falls back to `started_at.elapsed()`
    /// at finalize time — which is correct for streams since there's
    /// no failover loop inflating the e2e timer.
    pub picked_latency_ms: Option<u32>,
}

/// Record health for the picked route once the request finishes.
/// Best-effort; failures are logged.
pub(crate) async fn finalize_health(state: &GatewayState, sel: SelectionRecord, success: bool) {
    let total_latency_ms = sel.started_at.elapsed().as_millis().min(u32::MAX as u128) as u32;
    // For health, we want the picked route's *own* time, not the
    // cumulative including prior failed attempts. Non-stream sets
    // this explicitly when it picks a winner; stream falls back to
    // total elapsed (which is the picked route's time anyway since
    // streams don't loop).
    let health_latency_ms = sel.picked_latency_ms.unwrap_or(total_latency_ms);
    let breaker = resolve_breaker_config(state).await;
    let _ = state
        .health
        .record(sel.picked_route_id, health_latency_ms, !success, breaker)
        .await;
}

/// Returns true if the error is retryable.
///
/// Retry-eligible:
///   * NetworkError, ProviderTimeout — request didn't complete; the
///     same upstream might succeed on a second try.
///   * ProviderError, UpstreamRateLimited — historical catch-alls.
///   * ProviderHttpError 5xx — upstream had a transient issue.
///
/// Not retryable:
///   * ProviderHttpError 4xx (except 429) — the request itself is
///     poison; same upstream will reject again.
///   * ProviderInvalidResponse — upstream succeeded but the body is
///     unparseable; retrying the same upstream is pointless. Failover
///     to a different provider is still triggered upstream of this.
fn is_retryable(err: &GatewayError) -> bool {
    match err {
        GatewayError::NetworkError(_)
        | GatewayError::ProviderError(_)
        | GatewayError::ProviderTimeout(_)
        | GatewayError::UpstreamRateLimited { .. } => true,
        GatewayError::ProviderHttpError { status, .. } => *status >= 500 || *status == 408,
        _ => false,
    }
}

/// Non-streaming selection + failover. All routes are peers (no
/// priority tier in v2): `pick_with_strategy` picks one healthy
/// candidate, the proxy calls it, and on retryable error tries
/// another candidate from the remaining set until exhausted.
async fn select_route_with_failover<'a>(
    routes: &'a [RouteEntry],
    request: &ChatCompletionRequest,
    ctx: &SelectionCtx<'_>,
) -> Result<
    (
        &'a RouteEntry,
        crate::providers::traits::ChatCompletionResponse,
        SelectionRecord,
    ),
    GatewayError,
> {
    let started_at = std::time::Instant::now();
    let candidates: Vec<&RouteEntry> = routes.iter().collect();

    let mut last_error: Option<GatewayError> = None;
    let mut tried: Vec<Uuid> = Vec::new();

    for _ in 0..candidates.len() {
        let Some(entry) = pick_with_strategy(&candidates, &tried, ctx).await else {
            break;
        };
        tried.push(entry.provider_id);

        let mut req = request.clone();
        if let Some(ref upstream) = entry.upstream_model {
            req.model = upstream.clone();
        }

        // Per-attempt clock: record latency against this route as
        // *its* time, not "everything since the request started"
        // (which would double-count earlier failed attempts in
        // a failover chain and skew the latency strategy).
        let attempt_started_at = std::time::Instant::now();
        let result = entry.provider.chat_completion_boxed(req).await;
        let attempt_latency_ms = attempt_started_at
            .elapsed()
            .as_millis()
            .min(u32::MAX as u128) as u32;

        match result {
            Ok(response) => {
                set_affinity(
                    &ctx.state.redis,
                    ctx.user_id,
                    ctx.model_id,
                    ctx.affinity_mode,
                    entry,
                    ctx.affinity_ttl_secs,
                )
                .await;
                return Ok((
                    entry,
                    response,
                    SelectionRecord {
                        picked_route_id: entry.route_id,
                        started_at,
                        picked_latency_ms: Some(attempt_latency_ms),
                    },
                ));
            }
            Err(e) if is_retryable(&e) => {
                tracing::warn!(
                    provider = %entry.provider.name(),
                    provider_id = %entry.provider_id,
                    error = %e,
                    "Route failed, trying next"
                );
                metrics::counter!(
                    "gateway_provider_fallback_total",
                    "from" => crate::metrics_labels::normalize_provider_label(entry.provider.name()),
                )
                .increment(1);
                // Record the failed attempt in health so the
                // breaker can trip mid-failover.
                let _ = ctx
                    .state
                    .health
                    .record(entry.route_id, attempt_latency_ms, true, ctx.breaker)
                    .await;
                last_error = Some(e);
                continue;
            }
            Err(e) => {
                // Non-retryable — record health then bail. Sibling
                // providers will reject the same poison request.
                let _ = ctx
                    .state
                    .health
                    .record(entry.route_id, attempt_latency_ms, true, ctx.breaker)
                    .await;
                return Err(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::ProviderError(format!("All routes failed for model: {}", ctx.model_id))
    }))
}

/// Streaming variant: pick via strategy + health filter, but don't
/// call the provider — caller wires up the SSE stream and once the
/// first chunk lands a retry is no longer possible. Returns the
/// chosen entry plus a `SelectionRecord` so the streaming caller can
/// record health on stream completion.
async fn select_route_for_stream<'a>(
    routes: &'a [RouteEntry],
    ctx: &SelectionCtx<'_>,
) -> Result<(&'a RouteEntry, SelectionRecord), GatewayError> {
    let started_at = std::time::Instant::now();
    let candidates: Vec<&RouteEntry> = routes.iter().collect();

    if let Some(entry) = pick_with_strategy(&candidates, &[], ctx).await {
        return Ok((
            entry,
            SelectionRecord {
                picked_route_id: entry.route_id,
                started_at,
                // Streams don't loop — finalize_health falls
                // back to elapsed-from-started_at, which IS the
                // picked route's wall time for the streaming case.
                picked_latency_ms: None,
            },
        ));
    }

    Err(GatewayError::ProviderError(format!(
        "No provider found for model: {}",
        ctx.model_id
    )))
}

/// Build a human-readable label for "which rule rejected the request",
/// used as the error body so callers know what to lift. The label
/// shape is `<subject>:<metric>/<window>` (e.g. `api_key:tokens/1h`).
fn rule_label(rule: &limits::RateLimitRule) -> String {
    let window = match rule.window_secs {
        60 => "1m".to_string(),
        300 => "5m".to_string(),
        3_600 => "1h".to_string(),
        18_000 => "5h".to_string(),
        86_400 => "1d".to_string(),
        604_800 => "1w".to_string(),
        n => format!("{n}s"),
    };
    format!(
        "{}:{}/{}",
        rule.subject_kind.as_str(),
        rule.metric.as_str(),
        window
    )
}

/// POST /v1/chat/completions
///
/// Proxies chat completion requests to the appropriate AI provider based
/// on the model name in the request body. Supports both streaming (SSE)
/// and non-streaming (JSON) modes.
///
/// Request pipeline:
/// 1. Model mapping (aliases)
/// 2. Enforce allowed_models from API key (if set)
/// 3. Content filter (prompt injection detection)
/// 4. Token quota check
/// 5. Cache lookup (non-streaming only)
/// 6. Route to provider
/// 7. On success: consume quota, store cache, return response
pub async fn proxy_chat_completion(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(mut request): Json<ChatCompletionRequest>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    // 1. Apply model mapping
    request.model = state.model_mapper.map(&request.model);

    // Resolve trace_id and start clock up front so every early-return
    // path (allowed_models reject / preflight rate limit / content
    // filter block / route lookup miss) can emit a gateway_logs row
    // before bubbling. RequestMetadata::extract honors the same
    // x-trace-id header further down, so metadata.request_id ends up
    // matching trace_id.
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();
    let ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        session_id: session_id.clone(),
        user_id: identity.user_id.clone(),
        user_email: identity.user_email.clone(),
        api_key_id: identity.api_key_id.clone(),
        api_key_lineage_id: identity.api_key_lineage_id.clone(),
        ip_address: identity.ip_address.clone(),
        model: request.model.clone(),
        started: request_started_at,
    };

    // 2. Enforce allowed_models from API key
    if let Some(ref allowed) = identity.allowed_models
        && !allowed.is_empty()
        && !allowed
            .iter()
            .any(|m| request.model == *m || request.model.starts_with(m))
    {
        return Err(ctx
            .emit(GatewayError::TransformError(format!(
                "Model '{}' is not allowed for this API key",
                request.model
            )))
            .into());
    }

    // 3. Rate limit pre-flight (requests metric). See
    // `preflight_request_limits` — same path used by Anthropic
    // Messages and the Responses surface so all three obey the
    // same `(api_key, user, provider)` rule resolution and the
    // same `security.rate_limit_fail_closed` toggle.
    let (_provider_id, request_rules) = preflight_request_limits(&state, &identity, &request.model)
        .await
        .map_err(|e| GatewayErrorResponse::from(ctx.emit(e.0)))?;

    // 4. Extract per-request metadata from headers and body
    let metadata = RequestMetadata::extract(&headers, &request);
    tracing::info!(
        request_id = %metadata.request_id,
        model = %metadata.model,
        tags = ?metadata.tags,
        "Request metadata extracted"
    );

    // 4. Content filter — check for prompt injection
    let content_filter = state.content_filter.load();
    if let Some(m) = content_filter.check(&request.messages) {
        // Log lines use `log_summary()` (no matched snippet) so user
        // prompt content doesn't tunnel into the centralized log
        // pipeline. The client-facing error still uses the full
        // Display form so the caller can see what triggered the rule
        // and adjust their prompt — that surface is the user's own
        // request body, so showing it back is not a leak.
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {}", m.log_summary());
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => {
                tracing::warn!(
                    "Content filter warning (request allowed): {}",
                    m.log_summary()
                );
            }
            Action::Log => {
                tracing::info!("Content filter log: {}", m.log_summary());
            }
        }
    }

    // 5. Attach caller identity for custom header template resolution
    request.caller_user_id = identity.user_id.clone();
    request.caller_user_email = identity.user_email.clone();

    // 6. PII redaction — redact user messages before sending upstream.
    //    Placeholders are stable (no per-request salt) so two callers
    //    sending structurally-identical prompts produce identical
    //    redacted bodies. The cache keys on the redacted form: same
    //    structure ⇒ same key ⇒ shared cache slot. The cache stores
    //    the *unrestored* response (with placeholders intact); each
    //    retrieving caller restores using their own redaction context
    //    on the way out. Two callers with different PII embedded
    //    inside the same prompt structure each see their own values
    //    on restoration — symmetric and correct because upstream
    //    only ever saw the placeholder.
    let pii_redactor = state.pii_redactor.load();
    // Snapshot the pre-redaction messages so the audit pipeline can
    // capture what the user actually authored. Upstream sees the
    // redacted form, but the audit row is the legal record of
    // intent: "user X asked Y, gateway sent placeholder-substituted
    // form upstream". If we logged the post-redaction shape, the
    // audit trail would be sanitized in a way the auditor can't
    // un-sanitize (placeholders use stable salts shared by every
    // caller with the same prompt). The clone is per-request and
    // bounded by `audit.body_max_bytes`.
    let messages_for_audit = request.messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&request.messages);
    request.messages = redacted_messages;

    // 7. Check token quota — use user/api_key as quota key when available
    let quota_key = identity
        .user_id
        .as_deref()
        .or(identity.api_key_id.as_deref())
        .map(|id| format!("{id}:{}", request.model))
        .unwrap_or_else(|| request.model.clone());
    if let Err(e) = state.quota.check_quota(&quota_key).await {
        tracing::warn!("Quota exceeded for {quota_key}: {e}");
        return Err(ctx
            .emit(GatewayError::ProviderError(format!("Quota exceeded: {e}")))
            .into());
    }

    let is_stream = request.stream.unwrap_or(false);

    // Cache lookup — semantic cache shared across all users.
    // Both streaming and non-streaming paths check cache; on a hit
    // for a streaming request we re-emit the assembled response as
    // a single-chunk SSE stream so the client gets the format it
    // asked for.
    //
    // Two contracts the lookup enforces:
    //
    // 1. **Key by pre-redaction content** so identical user-visible
    //    prompts collide on the same cache slot regardless of whose
    //    PII the prompt contained. The stored response carries
    //    redaction placeholders (`{{EMAIL_1}}` etc.) and we restore
    //    using THIS caller's redaction context on the way out. Two
    //    callers with identical pre-redaction text MUST share
    //    identical redaction contexts (the PII values come from the
    //    text itself), so cross-caller restoration is symmetric.
    //
    // 2. **Cache hits debit quota** the same way an upstream call
    //    would have. The traditional "cache hits are free" reading
    //    lets a user with a deterministic prompt amortise a single
    //    real call across an unbounded quota window — i.e. quota
    //    enforcement becomes optional. Debit the cached
    //    `usage.total_tokens` so monthly caps still bind.
    if let Some(mut cached) = state.cache.get(&request).await {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
        tracing::debug!(model = %request.model, stream = is_stream, "Cache HIT");

        // (2) Quota — debit before serving the cached body so the user
        // can't trivially exceed their monthly cap through cached
        // round-trips. Quota errors here STILL serve the cached
        // response because we already passed the `check_quota` gate
        // at the top of the handler; treating consume as best-effort
        // matches the post-upstream path below at line 1463.
        if let Some(ref usage) = cached.usage
            && let Err(e) = state.quota.consume(&quota_key, usage.total_tokens).await
        {
            tracing::warn!(
                quota_key = %quota_key,
                tokens = usage.total_tokens,
                "quota consume on cache hit failed: {e}"
            );
        }

        // (1) Restore PII for this caller using their own redaction
        // context. The cached response carries opaque placeholders;
        // each consumer paints in their own values.
        pii_redactor.restore_response(&mut cached, &redaction_ctx);

        // Cache hits previously bypassed `gateway_logs` entirely, so
        // the bastion's audit story had a hole — "user X called model
        // Y" showed nothing for any deterministic prompt repeat. Emit
        // a gateway row with status `from_cache` so the audit timeline
        // is complete; cost is 0 because no upstream tokens were
        // spent (the original miss already booked them, the cache hit
        // is free). Request body is the user's actual prompt; response
        // is the cached completion.
        let (cached_pt, cached_ct) = cached
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        // Cache-hit body capture goes through the SAME pipeline as
        // fresh requests — PII redaction toggle, byte-cap truncation,
        // and S3 offload all apply. The prior shortcut here called
        // `serde_json::to_string` directly which broke three contracts:
        // (1) `audit.body_redact_pii=true` was silently ignored for
        // cache hits, (2) a multi-MB cached response landed inline in
        // CH without truncation, (3) oversize cached responses never
        // offloaded to S3 even when configured. Override the status
        // back to `from_cache` afterwards so auditors can still tell
        // these rows apart from fresh captures.
        let mut cache_body_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &metadata.request_id,
            &messages_for_audit,
            Some(&cached),
        )
        .await;
        cache_body_capture.status = Some(BODY_FROM_CACHE);
        emit_gateway_log(
            &state.audit,
            &metadata.request_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &request.model,
            None,
            None,
            cached_pt,
            cached_ct,
            Decimal::ZERO,
            request_started_at.elapsed().as_millis() as i64,
            200,
            cache_body_capture,
        );

        if is_stream {
            // Re-emit as SSE: one data chunk with the full response + [DONE]
            let chunk_json = serde_json::to_string(&cached).unwrap_or_default();
            let body = async_stream::stream! {
                yield Ok::<axum::response::sse::Event, Infallible>(
                    axum::response::sse::Event::default().data(chunk_json),
                );
                yield Ok::<axum::response::sse::Event, Infallible>(
                    axum::response::sse::Event::default().data("[DONE]"),
                );
            };
            let mut response = axum::response::sse::Sse::new(body).into_response();
            response
                .headers_mut()
                .insert("X-Cache", axum::http::HeaderValue::from_static("HIT"));
            response.headers_mut().insert(
                "X-Metadata-Request-Id",
                request_id_header(&metadata.request_id),
            );
            return Ok(response);
        }
        let mut response = Json(&cached).into_response();
        response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("HIT"));
        response.headers_mut().insert(
            "X-Metadata-Request-Id",
            request_id_header(&metadata.request_id),
        );
        return Ok(response);
    }
    metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);

    // Route to provider — multi-route failover
    let original_model = request.model.clone();
    let router = state.router.load();
    let routes = router.route(&request.model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {}",
            request.model
        )))
    })?;

    if is_stream {
        // Select route (with affinity) for streaming — no retry after
        // first chunk, so pick the best candidate up front. Route-
        // lookup failures on the streaming branch deserve a
        // gateway_logs row just like the non-streaming bubble above
        // — operators debugging "my SSE stream never started" would
        // otherwise find zero trace events to correlate against.
        let sel_ctx =
            build_selection_ctx(&state, &original_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        // Replace model with upstream_model if configured
        if let Some(ref upstream) = entry.upstream_model {
            request.model = upstream.clone();
        }

        set_affinity(
            &state.redis,
            identity.user_id.as_deref(),
            &original_model,
            sel_ctx.affinity_mode,
            entry,
            sel_ctx.affinity_ttl_secs,
        )
        .await;

        // Capture everything the post-flight callback needs BEFORE
        // moving `request` into the provider stream.
        let db = state.db.clone();
        let redis = state.redis.clone();
        let dynamic_config = state.dynamic_config.clone();
        let weight_cache = state.weight_cache.clone();
        let model = original_model.clone();
        let request_rules_for_done = request_rules.clone();
        let budget_caps = budgets_for_ai_gateway(&identity);
        // Extra captures for emit_gateway_log — cloning small strings
        // here is cheaper than retaining &state through the 'static bound.
        let audit_for_done = state.audit.clone();
        let cost_tracker = state.cost_tracker.clone();
        let trace_id_for_done = metadata.request_id.clone();
        let session_id_for_done = session_id.clone();
        let user_id_for_done = identity.user_id.clone();
        let user_email_for_done = identity.user_email.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let api_key_lineage_id_for_done = identity.api_key_lineage_id.clone();
        let ip_address_for_done = identity.ip_address.clone();
        let model_for_log = original_model.clone();
        let provider_name_for_done = entry.provider_name.clone();
        let upstream_model_for_done = entry.upstream_model.clone();
        let started = request_started_at;
        // Clone request for cache write — the original is moved into the provider.
        let request_for_cache = request.clone();
        // Snapshot the PRE-redaction messages into the on_done closure
        // so audit-time body capture sees what the user actually wrote
        // (request_for_cache holds the redacted form because it was
        // cloned AFTER the pii_redactor mutated request.messages).
        let messages_for_audit_for_done = messages_for_audit.clone();
        let cache_for_done = state.cache.clone();
        let state_for_done = state.clone();
        let stream = entry.provider.stream_chat_completion(request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                // Use the same token-resolution helper for the log row
                // AND the budget increment (computed once below for
                // both call sites). Previously the log row read raw
                // `result.usage` which is None on ClientCancelled
                // before the final usage chunk arrived — producing
                // gateway_logs rows with pt=0, ct=0, cost=0 even though
                // the upstream HAD generated and billed tokens. The
                // budget already used the estimate path; the log was
                // out of sync, breaking cost analytics for cancelled
                // streams.
                let (pt, ct) =
                    stream_usage_or_estimate(&result, &request_for_cache.messages);
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct).await;
                // Single classifier — Natural → 200, UpstreamError →
                // the underlying GatewayError's canonical status (not
                // the old 502 blanket), ClientCancelled → 499.
                let (logged_status, error_detail) = result.outcome.logged_status_and_detail();

                // Assemble the streamed chunks into the canonical
                // response shape ONCE — used both as the audit body
                // and (when the stream ran to natural completion) as
                // the cache fill below. Previously assemble_response
                // only ran on the cache-fill side, so the audit row
                // had no response payload to attach. Assembling for
                // every outcome (natural, error, cancelled) lets the
                // audit pipeline capture partial completions too,
                // which is exactly what auditors need for
                // mid-conversation incidents.
                let assembled_for_audit = crate::streaming::assemble_response(
                    &result.chunks,
                    result.usage.clone(),
                );
                let stream_body_capture = prepare_body_capture(
                    &state_for_done.dynamic_config,
                    &state_for_done.pii_redactor.load(),
                    &state_for_done.blob_store,
                    &trace_id_for_done,
                    &messages_for_audit_for_done,
                    assembled_for_audit.as_ref(),
                )
                .await;

                emit_gateway_log_with_extra(
                    &audit_for_done,
                    &trace_id_for_done,
                    session_id_for_done.as_deref(),
                    user_id_for_done.as_deref(),
                    user_email_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    api_key_lineage_id_for_done.as_deref(),
                    ip_address_for_done.as_deref(),
                    &model_for_log,
                    Some(provider_name_for_done.as_str()),
                    upstream_model_for_done.as_deref(),
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    logged_status,
                    error_detail,
                    stream_body_capture,
                );

                // Health: Natural and ClientCancelled count as
                // successes against the upstream (the latter is the
                // client's choice, not the upstream's failure).
                let stream_success = matches!(
                    result.outcome,
                    crate::streaming::StreamOutcome::Natural
                        | crate::streaming::StreamOutcome::ClientCancelled
                );
                finalize_health(&state_for_done, sel_record, stream_success).await;

                // Cache the assembled response when the stream ran to
                // natural completion (partial streams from client
                // disconnects are NOT cached). Reuses
                // `assembled_for_audit` so we don't pay the assembly
                // cost twice for a successful stream.
                if result.natural_completion
                    && let Some(ref assembled) = assembled_for_audit
                {
                    cache_for_done
                        .set(&request_for_cache, assembled, None)
                        .await;
                }

                // Reuse the (pt, ct) computed above for the log row so
                // budget enforcement and cost analytics agree on the
                // token count — without this, a cancelled stream would
                // bill the budget for the estimated tokens but the
                // gateway_logs row would show 0 tokens and 0 cost,
                // making the two views permanently inconsistent.
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model,
                    pt,
                    ct,
                    request_rules_for_done,
                    budget_caps.clone(),
                    user_id_for_done,
                    user_email_for_done,
                    api_key_id_for_done,
                    ip_address_for_done,
                    audit_for_done,
                )
                .await;
            })
        };
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        Ok(stream_to_sse_with_restorer(stream, on_done, stream_restorer).into_response())
    } else {
        // Non-streaming: full failover with retry across healthy candidates
        let sel_ctx =
            build_selection_ctx(&state, &original_model, identity.user_id.as_deref()).await;
        // Prepare the error-path body capture BEFORE select_route_with_failover
        // so the (synchronous) map_err closure can move it in without
        // needing to await. Error paths capture the request body only —
        // there's no response from any upstream that succeeded.
        let error_path_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &metadata.request_id,
            &messages_for_audit,
            None,
        )
        .await;
        let (chosen_entry, mut response, sel_record) =
            select_route_with_failover(routes, &request, &sel_ctx)
                .await
                .map_err(|e| {
                    // select_route_with_failover just errored across every
                    // candidate — there's no winning provider to attribute
                    // this failure to, so provider stays None.
                    emit_gateway_error_log(
                        &state.audit,
                        &metadata.request_id,
                        session_id.as_deref(),
                        identity.user_id.as_deref(),
                        identity.user_email.as_deref(),
                        identity.api_key_id.as_deref(),
                        identity.api_key_lineage_id.as_deref(),
                        identity.ip_address.as_deref(),
                        &original_model,
                        None,
                        request_started_at.elapsed().as_millis() as i64,
                        &e,
                        error_path_capture,
                    );
                    GatewayErrorResponse::from(e)
                })?;

        // Restore original model name in response (don't leak upstream_model)
        response.model = original_model.clone();

        // 8a-pre. Output guardrails — enforce per-model size / shape
        // caps on the assistant message before it reaches the caller.
        // Runs BEFORE PII restore because the rule operates on raw
        // completion text; running it after would let a redaction
        // placeholder push a legitimate completion past the cap.
        // Streaming guardrails would require buffering the whole
        // stream, which fights latency — non-streaming only for now.
        let model_cfg = router.config_for(&original_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        // 8a. Cache the *unrestored* response BEFORE PII restoration
        // so future cache hits can restore using their own caller's
        // redaction context. Storing the restored form would bake
        // this caller's PII into the shared cache entry. The key is
        // computed from the *redacted* request body, so two callers
        // sending structurally-identical prompts (even with different
        // PII embedded) collide on the same slot.
        state.cache.set(&request, &response, None).await;

        // 8b. Restore PII in the response (this caller's view).
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // 8c. Consume quota based on actual token usage
        if let Some(ref usage) = response.usage {
            let total = usage.total_tokens;
            if let Err(e) = state.quota.consume(&quota_key, total).await {
                tracing::warn!("Failed to consume quota: {e}");
            }
        }

        // 8d. Post-flight accounting against the limits engine.
        if let Some(ref usage) = response.usage {
            post_flight_account(
                state.db.clone(),
                state.redis.clone(),
                state.dynamic_config.clone(),
                state.weight_cache.clone(),
                original_model.clone(),
                usage.prompt_tokens,
                usage.completion_tokens,
                request_rules.clone(),
                budgets_for_ai_gateway(&identity),
                identity.user_id.clone(),
                identity.user_email.clone(),
                identity.api_key_id.clone(),
                identity.ip_address.clone(),
                state.audit.clone(),
            )
            .await;
        }

        // 8e. Log audit detail including metadata
        tracing::info!(
            request_id = %metadata.request_id,
            metadata = %metadata.to_json(),
            "Audit log: request completed"
        );

        // 8f. Emit gateway_logs row so `GET /api/admin/trace/{id}` can
        // find this request. Cost is computed from provider pricing if
        // we know it, otherwise 0.0 (analytics treats NULL == 0).
        let (prompt_tokens, completion_tokens) = response
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let cost_usd = state
            .cost_tracker
            .calculate_cost(&original_model, prompt_tokens, completion_tokens)
            .await;
        let body_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &metadata.request_id,
            &messages_for_audit,
            Some(&response),
        )
        .await;
        emit_gateway_log(
            &state.audit,
            &metadata.request_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &original_model,
            Some(chosen_entry.provider_name.as_str()),
            chosen_entry.upstream_model.as_deref(),
            prompt_tokens,
            completion_tokens,
            cost_usd,
            request_started_at.elapsed().as_millis() as i64,
            200,
            body_capture,
        );

        finalize_health(&state, sel_record, true).await;

        let mut http_response = Json(&response).into_response();
        http_response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("MISS"));
        http_response.headers_mut().insert(
            "X-Metadata-Request-Id",
            request_id_header(&metadata.request_id),
        );
        Ok(http_response)
    }
}

/// Build a `HeaderValue` from a request id, falling back to a placeholder if
/// the id contains bytes outside the printable-ASCII range. Validation in
/// `RequestMetadata::extract` should prevent this, but we never want a
/// malformed id to crash the response path.
fn request_id_header(request_id: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(request_id)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("invalid"))
}

/// GET /v1/models
///
/// Returns the list of available models in OpenAI-compatible format.
pub async fn list_models_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let models = state.router.load().list_models();

    let model_objects: Vec<serde_json::Value> = models
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "think-watch",
            })
        })
        .collect();

    Json(serde_json::json!({
        "object": "list",
        "data": model_objects,
    }))
}

/// POST /v1/messages
///
/// Anthropic Messages API passthrough. Used by Claude Code and other tools
/// that speak the Anthropic native format. Routes to the provider registered
/// for the requested model, forwarding the request as-is to the Anthropic
/// upstream (no format conversion needed).
///
/// This endpoint also applies content filtering, quota checks, and audit
/// logging, but does NOT do PII redaction or caching (complex content types).
pub async fn proxy_anthropic_messages(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    // Honor x-trace-id when the caller pinned one — that's how a
    // client correlates this AI call with the MCP tools/call it
    // makes off the back of a tool-use response. Otherwise mint.
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();

    // Build LogCtx with model="(unknown)" up-front so a missing-model
    // body still emits an error row. We rebuild it once we know the
    // real model so subsequent emits attribute correctly.
    let early_ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        session_id: session_id.clone(),
        user_id: identity.user_id.clone(),
        user_email: identity.user_email.clone(),
        api_key_id: identity.api_key_id.clone(),
        api_key_lineage_id: identity.api_key_lineage_id.clone(),
        ip_address: identity.ip_address.clone(),
        model: "(unknown)".into(),
        started: request_started_at,
    };
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            early_ctx.emit(GatewayError::TransformError("Missing 'model' field".into()))
        })?
        .to_string();

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Apply model mapping
    let mapped_model = state.model_mapper.map(&model);
    let ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        session_id: session_id.clone(),
        user_id: identity.user_id.clone(),
        user_email: identity.user_email.clone(),
        api_key_id: identity.api_key_id.clone(),
        api_key_lineage_id: identity.api_key_lineage_id.clone(),
        ip_address: identity.ip_address.clone(),
        model: mapped_model.clone(),
        started: request_started_at,
    };

    // Rate limit pre-flight — same engine as the chat-completions
    // path so a developer key can't dodge their per-minute quota
    // by switching from `/v1/chat/completions` to `/v1/messages`.
    let (_provider_id, request_rules) = preflight_request_limits(&state, &identity, &mapped_model)
        .await
        .map_err(|e| GatewayErrorResponse::from(ctx.emit(e.0)))?;

    // Content filter — check user messages
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        let chat_messages: Vec<crate::providers::traits::ChatMessage> = messages
            .iter()
            .filter_map(|m| {
                Some(crate::providers::traits::ChatMessage {
                    role: m.get("role")?.as_str()?.to_string(),
                    content: m.get("content").cloned().unwrap_or(serde_json::Value::Null),
                    ..Default::default()
                })
            })
            .collect();

        let content_filter = state.content_filter.load();
        if let Some(m) = content_filter.check(&chat_messages) {
            match m.action {
                Action::Block => {
                    tracing::warn!("Content filter blocked request: {m}");
                    return Err(ctx
                        .emit(GatewayError::TransformError(format!(
                            "Request blocked by content filter: {m}"
                        )))
                        .into());
                }
                Action::Warn => tracing::warn!("Content filter warning: {m}"),
                Action::Log => tracing::info!("Content filter log: {m}"),
            }
        }
    }

    // Route to provider — multi-route failover
    let router = state.router.load();
    let routes = router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;

    // Convert to OpenAI format internally, let the provider handle the rest
    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    // Build a ChatCompletionRequest from the Anthropic body
    let mut messages = Vec::new();
    if let Some(system) = body.get("system").and_then(|v| v.as_str()) {
        messages.push(crate::providers::traits::ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(system.to_string()),
            ..Default::default()
        });
    }
    if let Some(msg_array) = body.get("messages").and_then(|v| v.as_array()) {
        for m in msg_array {
            if let (Some(role), Some(content)) =
                (m.get("role").and_then(|v| v.as_str()), m.get("content"))
            {
                messages.push(crate::providers::traits::ChatMessage {
                    role: role.to_string(),
                    content: content.clone(),
                    ..Default::default()
                });
            }
        }
    }

    // PII redaction
    let pii_redactor = state.pii_redactor.load();
    // Snapshot pre-redaction messages for the audit body-capture
    // pipeline. Same reasoning as the chat-completions handler: the
    // audit row needs to show what the user actually wrote.
    let messages_for_audit = messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&messages);

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages: redacted_messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
        caller_user_id: identity.user_id.clone(),
        caller_user_email: identity.user_email.clone(),
        trace_id: Some(trace_id.clone()),
    };

    if is_stream {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        let mut stream_request = request.clone();
        if let Some(ref upstream) = entry.upstream_model {
            stream_request.model = upstream.clone();
        }

        set_affinity(
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            sel_ctx.affinity_mode,
            entry,
            sel_ctx.affinity_ttl_secs,
        )
        .await;

        let db = state.db.clone();
        let redis = state.redis.clone();
        let dynamic_config = state.dynamic_config.clone();
        let weight_cache = state.weight_cache.clone();
        let model_for_done = mapped_model.clone();
        let request_rules_for_done = request_rules.clone();
        let budget_caps = budgets_for_ai_gateway(&identity);
        let audit_for_done = state.audit.clone();
        let cost_tracker = state.cost_tracker.clone();
        let trace_id_for_done = trace_id.clone();
        let session_id_for_done = session_id.clone();
        // Capture the prompt messages for usage estimation when the
        // upstream stream doesn't surface a usage chunk — see
        // `stream_usage_or_estimate` for the budget-bypass context.
        let messages_for_done = request.messages.clone();
        // Pre-redaction snapshot for audit body capture (see the
        // `messages_for_audit` clone at the top of the handler).
        let messages_for_audit_for_done = messages_for_audit.clone();
        let user_id_for_done = identity.user_id.clone();
        let user_email_for_done = identity.user_email.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let api_key_lineage_id_for_done = identity.api_key_lineage_id.clone();
        let ip_address_for_done = identity.ip_address.clone();
        let model_for_log = mapped_model.clone();
        let provider_name_for_done = entry.provider_name.clone();
        let upstream_model_for_done = entry.upstream_model.clone();
        let started = request_started_at;
        let state_for_done = state.clone();
        let stream = entry.provider.stream_chat_completion(stream_request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                // Resolve tokens once via the estimator (handles the
                // ClientCancelled / no-final-usage case) and share the
                // result between the log row and the budget update so
                // the two views agree.
                let (pt, ct) = stream_usage_or_estimate(&result, &messages_for_done);
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct).await;
                let (logged_status, error_detail) = result.outcome.logged_status_and_detail();
                // Assemble streamed chunks into a canonical response for
                // body capture. Same logic as the chat-completions stream
                // path; auditors get the user's actual prompt + the AI's
                // (possibly partial) reply.
                let assembled_for_audit = crate::streaming::assemble_response(
                    &result.chunks,
                    result.usage.clone(),
                );
                let stream_body_capture = prepare_body_capture(
                    &state_for_done.dynamic_config,
                    &state_for_done.pii_redactor.load(),
                    &state_for_done.blob_store,
                    &trace_id_for_done,
                    &messages_for_audit_for_done,
                    assembled_for_audit.as_ref(),
                )
                .await;
                emit_gateway_log_with_extra(
                    &audit_for_done,
                    &trace_id_for_done,
                    session_id_for_done.as_deref(),
                    user_id_for_done.as_deref(),
                    user_email_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    api_key_lineage_id_for_done.as_deref(),
                    ip_address_for_done.as_deref(),
                    &model_for_log,
                    Some(provider_name_for_done.as_str()),
                    upstream_model_for_done.as_deref(),
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    logged_status,
                    error_detail,
                    stream_body_capture,
                );
                let stream_success = matches!(
                    result.outcome,
                    crate::streaming::StreamOutcome::Natural
                        | crate::streaming::StreamOutcome::ClientCancelled
                );
                finalize_health(&state_for_done, sel_record, stream_success).await;
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model_for_done,
                    pt,
                    ct,
                    request_rules_for_done,
                    budget_caps.clone(),
                    user_id_for_done,
                    user_email_for_done,
                    api_key_id_for_done,
                    ip_address_for_done,
                    audit_for_done,
                )
                .await;
            })
        };
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        let mut http_response =
            stream_to_sse_with_restorer(stream, on_done, stream_restorer).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    } else {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        // See chat-completions handler for rationale: pre-prepare the
        // error-path body capture so the synchronous map_err closure
        // can move it in.
        let error_path_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &trace_id,
            &messages_for_audit,
            None,
        )
        .await;
        let (chosen_entry, mut response, sel_record) =
            select_route_with_failover(routes, &request, &sel_ctx)
                .await
                .map_err(|e| {
                    emit_gateway_error_log(
                        &state.audit,
                        &trace_id,
                        session_id.as_deref(),
                        identity.user_id.as_deref(),
                        identity.user_email.as_deref(),
                        identity.api_key_id.as_deref(),
                        identity.api_key_lineage_id.as_deref(),
                        identity.ip_address.as_deref(),
                        &mapped_model,
                        None,
                        request_started_at.elapsed().as_millis() as i64,
                        &e,
                        error_path_capture,
                    );
                    GatewayErrorResponse::from(e)
                })?;

        // Restore original model name
        response.model = mapped_model.clone();

        // Output guardrails — same hook as the chat-completions surface;
        // see proxy_chat_completion for rationale on ordering vs. PII
        // restore and the streaming carve-out.
        let model_cfg = router.config_for(&mapped_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // Post-flight: same accounting path the chat-completions
        // surface uses. Anthropic responses always carry usage
        // unless the upstream errored.
        if let Some(ref usage) = response.usage {
            post_flight_account(
                state.db.clone(),
                state.redis.clone(),
                state.dynamic_config.clone(),
                state.weight_cache.clone(),
                mapped_model.clone(),
                usage.prompt_tokens,
                usage.completion_tokens,
                request_rules.clone(),
                budgets_for_ai_gateway(&identity),
                identity.user_id.clone(),
                identity.user_email.clone(),
                identity.api_key_id.clone(),
                identity.ip_address.clone(),
                state.audit.clone(),
            )
            .await;
        }

        // Emit gateway_logs for the trace timeline.
        let (pt, ct) = response
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let cost = state
            .cost_tracker
            .calculate_cost(&mapped_model, pt, ct)
            .await;
        let body_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &trace_id,
            &messages_for_audit,
            Some(&response),
        )
        .await;
        emit_gateway_log(
            &state.audit,
            &trace_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &mapped_model,
            Some(chosen_entry.provider_name.as_str()),
            chosen_entry.upstream_model.as_deref(),
            pt,
            ct,
            cost,
            request_started_at.elapsed().as_millis() as i64,
            200,
            body_capture,
        );

        finalize_health(&state, sel_record, true).await;

        // Convert OpenAI response back to Anthropic format
        let anthropic_response = convert_to_anthropic_response(&response);
        let mut http_response = Json(anthropic_response).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    }
}

/// Convert an OpenAI-format response back to Anthropic Messages API format.
fn convert_to_anthropic_response(
    resp: &crate::providers::traits::ChatCompletionResponse,
) -> serde_json::Value {
    let content: Vec<serde_json::Value> = resp
        .choices
        .iter()
        .map(|c| {
            let text = c.message.content.as_str().unwrap_or("").to_string();
            serde_json::json!({
                "type": "text",
                "text": text,
            })
        })
        .collect();

    let stop_reason = resp
        .choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(|r| match r {
            "stop" => "end_turn",
            "length" => "max_tokens",
            other => other,
        })
        .unwrap_or("end_turn");

    let (input_tokens, output_tokens) = resp
        .usage
        .as_ref()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));

    serde_json::json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    })
}

/// POST /v1/responses
///
/// OpenAI Responses API (new format, 2025+). Supports tool use, multi-turn,
/// and structured outputs natively. ThinkWatch proxies this by converting
/// to internal ChatCompletionRequest format, routing through the same
/// provider pipeline, then converting the response back.
///
/// For providers that support the Responses API natively (OpenAI), this
/// could be a direct passthrough in the future.
pub async fn proxy_responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();

    let early_ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        session_id: session_id.clone(),
        user_id: identity.user_id.clone(),
        user_email: identity.user_email.clone(),
        api_key_id: identity.api_key_id.clone(),
        api_key_lineage_id: identity.api_key_lineage_id.clone(),
        ip_address: identity.ip_address.clone(),
        model: "(unknown)".into(),
        started: request_started_at,
    };
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            early_ctx.emit(GatewayError::TransformError("Missing 'model' field".into()))
        })?
        .to_string();

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mapped_model = state.model_mapper.map(&model);
    let ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        session_id: session_id.clone(),
        user_id: identity.user_id.clone(),
        user_email: identity.user_email.clone(),
        api_key_id: identity.api_key_id.clone(),
        api_key_lineage_id: identity.api_key_lineage_id.clone(),
        ip_address: identity.ip_address.clone(),
        model: mapped_model.clone(),
        started: request_started_at,
    };

    // Rate limit pre-flight — same engine as the chat completions
    // path. Keeps the three AI surfaces symmetric.
    let (_provider_id, request_rules) = preflight_request_limits(&state, &identity, &mapped_model)
        .await
        .map_err(|e| GatewayErrorResponse::from(ctx.emit(e.0)))?;

    // Extract messages from the "input" field (Responses API format)
    // Input can be a string or an array of messages
    let mut messages = Vec::new();

    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        messages.push(crate::providers::traits::ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(instructions.to_string()),
            ..Default::default()
        });
    }

    match body.get("input") {
        Some(serde_json::Value::String(s)) => {
            messages.push(crate::providers::traits::ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(s.clone()),
                ..Default::default()
            });
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                // Each item can be a message object or a string
                if let Some(s) = item.as_str() {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::String(s.to_string()),
                        ..Default::default()
                    });
                } else if let (Some(role), Some(content)) = (
                    item.get("role").and_then(|v| v.as_str()),
                    item.get("content"),
                ) {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: role.to_string(),
                        content: content.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        _ => {
            return Err(ctx
                .emit(GatewayError::TransformError(
                    "Missing or invalid 'input' field".into(),
                ))
                .into());
        }
    }

    // Content filter
    let content_filter = state.content_filter.load();
    if let Some(m) = content_filter.check(&messages) {
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {m}");
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => tracing::warn!("Content filter warning: {m}"),
            Action::Log => tracing::info!("Content filter log: {m}"),
        }
    }

    // PII redaction — same pipeline the chat-completions and Anthropic
    // surfaces use, so /v1/responses doesn't leak emails / phones / IDs
    // upstream just because it's the third-class endpoint. Streaming
    // restoration runs through PiiStreamRestorer below; non-streaming
    // restoration runs against the converted response right before we
    // hand it back to the client.
    let pii_redactor = state.pii_redactor.load();
    let messages_for_audit = messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&messages);

    let max_tokens = body
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages: redacted_messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
        caller_user_id: identity.user_id.clone(),
        caller_user_email: identity.user_email.clone(),
        trace_id: Some(trace_id.clone()),
    };

    // Route to provider — multi-route failover
    let router = state.router.load();
    let routes = router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;

    if is_stream {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        let mut stream_request = request.clone();
        if let Some(ref upstream) = entry.upstream_model {
            stream_request.model = upstream.clone();
        }

        set_affinity(
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            sel_ctx.affinity_mode,
            entry,
            sel_ctx.affinity_ttl_secs,
        )
        .await;

        let db = state.db.clone();
        let redis = state.redis.clone();
        let dynamic_config = state.dynamic_config.clone();
        let weight_cache = state.weight_cache.clone();
        let model_for_done = mapped_model.clone();
        let request_rules_for_done = request_rules.clone();
        let budget_caps = budgets_for_ai_gateway(&identity);
        let audit_for_done = state.audit.clone();
        let cost_tracker = state.cost_tracker.clone();
        let trace_id_for_done = trace_id.clone();
        let session_id_for_done = session_id.clone();
        // Capture the prompt messages for usage estimation when the
        // upstream stream doesn't surface a usage chunk — see
        // `stream_usage_or_estimate` for the budget-bypass context.
        let messages_for_done = request.messages.clone();
        // Pre-redaction snapshot for audit body capture (see the
        // `messages_for_audit` clone at the top of the handler).
        let messages_for_audit_for_done = messages_for_audit.clone();
        let user_id_for_done = identity.user_id.clone();
        let user_email_for_done = identity.user_email.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let api_key_lineage_id_for_done = identity.api_key_lineage_id.clone();
        let ip_address_for_done = identity.ip_address.clone();
        let model_for_log = mapped_model.clone();
        let provider_name_for_done = entry.provider_name.clone();
        let upstream_model_for_done = entry.upstream_model.clone();
        let started = request_started_at;
        let state_for_done = state.clone();
        let stream = entry.provider.stream_chat_completion(stream_request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                // Resolve tokens once via the estimator (handles the
                // ClientCancelled / no-final-usage case) and share the
                // result between the log row and the budget update so
                // the two views agree.
                let (pt, ct) = stream_usage_or_estimate(&result, &messages_for_done);
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct).await;
                let (logged_status, error_detail) = result.outcome.logged_status_and_detail();
                // Assemble streamed chunks into a canonical response for
                // body capture. Same logic as the chat-completions stream
                // path; auditors get the user's actual prompt + the AI's
                // (possibly partial) reply.
                let assembled_for_audit = crate::streaming::assemble_response(
                    &result.chunks,
                    result.usage.clone(),
                );
                let stream_body_capture = prepare_body_capture(
                    &state_for_done.dynamic_config,
                    &state_for_done.pii_redactor.load(),
                    &state_for_done.blob_store,
                    &trace_id_for_done,
                    &messages_for_audit_for_done,
                    assembled_for_audit.as_ref(),
                )
                .await;
                emit_gateway_log_with_extra(
                    &audit_for_done,
                    &trace_id_for_done,
                    session_id_for_done.as_deref(),
                    user_id_for_done.as_deref(),
                    user_email_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    api_key_lineage_id_for_done.as_deref(),
                    ip_address_for_done.as_deref(),
                    &model_for_log,
                    Some(provider_name_for_done.as_str()),
                    upstream_model_for_done.as_deref(),
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    logged_status,
                    error_detail,
                    stream_body_capture,
                );
                let stream_success = matches!(
                    result.outcome,
                    crate::streaming::StreamOutcome::Natural
                        | crate::streaming::StreamOutcome::ClientCancelled
                );
                finalize_health(&state_for_done, sel_record, stream_success).await;
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model_for_done,
                    pt,
                    ct,
                    request_rules_for_done,
                    budget_caps.clone(),
                    user_id_for_done,
                    user_email_for_done,
                    api_key_id_for_done,
                    ip_address_for_done,
                    audit_for_done,
                )
                .await;
            })
        };
        // Stitch placeholders back together as chunks stream through.
        // Same restorer the chat-completions surface uses; no-op when
        // redaction_ctx is empty so the feature-off path stays free.
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        let mut http_response =
            stream_to_sse_with_restorer(stream, on_done, stream_restorer).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    } else {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        // See chat-completions handler for rationale: pre-prepare the
        // error-path body capture so the synchronous map_err closure
        // can move it in.
        let error_path_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &trace_id,
            &messages_for_audit,
            None,
        )
        .await;
        let (chosen_entry, mut response, sel_record) =
            select_route_with_failover(routes, &request, &sel_ctx)
                .await
                .map_err(|e| {
                    emit_gateway_error_log(
                        &state.audit,
                        &trace_id,
                        session_id.as_deref(),
                        identity.user_id.as_deref(),
                        identity.user_email.as_deref(),
                        identity.api_key_id.as_deref(),
                        identity.api_key_lineage_id.as_deref(),
                        identity.ip_address.as_deref(),
                        &mapped_model,
                        None,
                        request_started_at.elapsed().as_millis() as i64,
                        &e,
                        error_path_capture,
                    );
                    GatewayErrorResponse::from(e)
                })?;

        // Restore original model name
        response.model = mapped_model.clone();

        // Output guardrails — see proxy_chat_completion for ordering.
        let model_cfg = router.config_for(&mapped_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        // Restore PII placeholders so the converted response carries
        // the original user data the model echoed back.
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        if let Some(ref usage) = response.usage {
            post_flight_account(
                state.db.clone(),
                state.redis.clone(),
                state.dynamic_config.clone(),
                state.weight_cache.clone(),
                mapped_model.clone(),
                usage.prompt_tokens,
                usage.completion_tokens,
                request_rules.clone(),
                budgets_for_ai_gateway(&identity),
                identity.user_id.clone(),
                identity.user_email.clone(),
                identity.api_key_id.clone(),
                identity.ip_address.clone(),
                state.audit.clone(),
            )
            .await;
        }

        let (pt, ct) = response
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let cost = state
            .cost_tracker
            .calculate_cost(&mapped_model, pt, ct)
            .await;
        let body_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &trace_id,
            &messages_for_audit,
            Some(&response),
        )
        .await;
        emit_gateway_log(
            &state.audit,
            &trace_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &mapped_model,
            Some(chosen_entry.provider_name.as_str()),
            chosen_entry.upstream_model.as_deref(),
            pt,
            ct,
            cost,
            request_started_at.elapsed().as_millis() as i64,
            200,
            body_capture,
        );

        finalize_health(&state, sel_record, true).await;

        let responses_format = convert_to_responses_format(&response);
        let mut http_response = Json(responses_format).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    }
}

/// Convert an internal ChatCompletionResponse to OpenAI Responses API format.
fn convert_to_responses_format(
    resp: &crate::providers::traits::ChatCompletionResponse,
) -> serde_json::Value {
    let mut output = Vec::new();

    for choice in &resp.choices {
        let text = choice.message.content.as_str().unwrap_or("").to_string();
        output.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4()),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
            }],
        }));
    }

    let (input_tokens, output_tokens) = resp
        .usage
        .as_ref()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));

    serde_json::json!({
        "id": resp.id,
        "object": "response",
        "created_at": resp.created,
        "status": "completed",
        "model": resp.model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

// ---------- Error adapter ----------

/// Newtype wrapper so we can implement `IntoResponse` for `GatewayError`.
pub struct GatewayErrorResponse(GatewayError);

impl From<GatewayError> for GatewayErrorResponse {
    fn from(err: GatewayError) -> Self {
        Self(err)
    }
}

impl IntoResponse for GatewayErrorResponse {
    fn into_response(self) -> axum::response::Response {
        use axum::http::{HeaderValue, StatusCode, header};

        let status =
            StatusCode::from_u16(self.0.status_code() as u16).unwrap_or(StatusCode::BAD_GATEWAY);
        let error_type = match &self.0 {
            GatewayError::ProviderError(_) => "provider_error",
            GatewayError::ProviderHttpError { .. } => "provider_http_error",
            GatewayError::ProviderTimeout(_) => "provider_timeout",
            GatewayError::ProviderInvalidResponse(_) => "provider_invalid_response",
            GatewayError::TransformError(_) => "transform_error",
            GatewayError::NetworkError(_) => "network_error",
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited(_) => {
                "rate_limited"
            }
            GatewayError::UpstreamAuthError => "auth_error",
        };

        let retry_after = self.0.retry_after_secs();
        let body = serde_json::json!({
            "error": {
                "message": self.0.to_string(),
                "type": error_type,
            }
        });

        let mut response = (status, Json(body)).into_response();
        // Echo the upstream's Retry-After (or our local default) so
        // well-behaved clients back off the right amount instead of
        // burning quota with tight 3× retries that all hit the same
        // open window.
        if let Some(secs) = retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    use axum::http::HeaderMap;

    /// `gateway_error_status` must agree with the HTTP status that
    /// `GatewayErrorResponse::into_response` returns — drift between
    /// them would make the gateway_logs `status_code` field disagree
    /// with the actual response code, and trace UI users would chase
    /// phantom 502s for what was actually a 429.
    #[test]
    fn gateway_error_status_matches_response_status() {
        for (err, expected) in [
            (GatewayError::ProviderError("x".into()), 502),
            (
                GatewayError::ProviderHttpError {
                    status: 418,
                    message: "teapot".into(),
                },
                418,
            ),
            (GatewayError::ProviderTimeout("x".into()), 504),
            (GatewayError::ProviderInvalidResponse("x".into()), 502),
            (GatewayError::TransformError("x".into()), 400),
            (GatewayError::NetworkError("x".into()), 502),
            (
                GatewayError::UpstreamRateLimited {
                    retry_after_secs: None,
                },
                429,
            ),
            (
                GatewayError::UpstreamRateLimited {
                    retry_after_secs: Some(45),
                },
                429,
            ),
            (GatewayError::LocalRateLimited("rule".into()), 429),
            (GatewayError::UpstreamAuthError, 401),
        ] {
            assert_eq!(
                gateway_error_status(&err),
                expected,
                "gateway_error_status mismatch for {err:?}"
            );
            // The wire-status path goes through IntoResponse — exercise
            // it so the two stay in lock-step even after either side is
            // refactored.
            let wire_status = GatewayErrorResponse::from(err)
                .into_response()
                .status()
                .as_u16() as i64;
            assert_eq!(
                wire_status, expected,
                "IntoResponse wire status disagrees with gateway_error_status"
            );
        }
    }

    /// The streaming on_done path historically hard-coded 502 for any
    /// mid-stream upstream failure, so a 429 from OpenRouter would
    /// land in gateway_logs as 502 and the dashboard would paint a
    /// healthy-but-throttled upstream red. Lock in that
    /// `StreamOutcome::UpstreamError` carries the underlying
    /// GatewayError's canonical status verbatim.
    #[test]
    fn stream_outcome_upstream_error_preserves_status() {
        use crate::streaming::StreamOutcome;
        for err in [
            GatewayError::UpstreamRateLimited {
                retry_after_secs: Some(12),
            },
            GatewayError::UpstreamAuthError,
            GatewayError::ProviderTimeout("slow".into()),
            GatewayError::ProviderError("boom".into()),
            GatewayError::ProviderHttpError {
                status: 503,
                message: "down".into(),
            },
        ] {
            let expected = err.status_code();
            let outcome = StreamOutcome::UpstreamError {
                error_type: err.error_tag().to_string(),
                message: err.to_string(),
                status_code: err.status_code(),
            };
            let (logged_status, detail) = outcome.logged_status_and_detail();
            assert_eq!(
                logged_status, expected,
                "stream-path status drift for {err:?}: got {logged_status}, expected {expected}"
            );
            assert!(
                detail.as_ref().and_then(|v| v.get("error_type")).is_some(),
                "stream outcome detail must carry error_type label"
            );
        }
    }

    #[test]
    fn stream_outcome_natural_and_cancelled_have_canonical_status() {
        use crate::streaming::StreamOutcome;
        assert_eq!(StreamOutcome::Natural.logged_status_and_detail().0, 200);
        assert_eq!(
            StreamOutcome::ClientCancelled.logged_status_and_detail().0,
            499
        );
    }

    /// 429 responses MUST carry a `Retry-After` header. Without one,
    /// naive SDKs (the field-observed case that triggered this fix)
    /// retry tightly and re-burn quota that was about to refill —
    /// turning a brief throttle into sustained pain. The upstream's
    /// own value wins; we fall back to a conservative default for
    /// local-limited responses.
    #[test]
    fn rate_limit_responses_carry_retry_after_header() {
        // Upstream provided a hint — echo it.
        let resp = GatewayErrorResponse::from(GatewayError::UpstreamRateLimited {
            retry_after_secs: Some(45),
        })
        .into_response();
        assert_eq!(resp.status().as_u16(), 429);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("45")
        );

        // Upstream silent — we still echo nothing, because we don't
        // know when the quota window opens. (The local-default only
        // applies to OUR own rate limiter, where we DO know.)
        let resp = GatewayErrorResponse::from(GatewayError::UpstreamRateLimited {
            retry_after_secs: None,
        })
        .into_response();
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none(),
            "no header when upstream didn't tell us — guessing would mislead clients"
        );

        // Local limit — we set our own conservative default so SDKs
        // see a number instead of immediately retrying.
        let resp = GatewayErrorResponse::from(GatewayError::LocalRateLimited("budget".into()))
            .into_response();
        assert_eq!(resp.status().as_u16(), 429);
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_some(),
            "local rate-limit must carry a Retry-After default"
        );

        // Non-429 responses must NOT carry Retry-After — would
        // confuse SDKs that special-case the header.
        let resp =
            GatewayErrorResponse::from(GatewayError::ProviderError("boom".into())).into_response();
        assert_eq!(resp.status().as_u16(), 502);
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
    }

    #[test]
    fn retry_after_parser_handles_delta_seconds_and_garbage() {
        use crate::providers::traits::parse_retry_after_seconds;
        assert_eq!(parse_retry_after_seconds("30"), Some(30));
        assert_eq!(parse_retry_after_seconds("  45  "), Some(45));
        assert_eq!(parse_retry_after_seconds("0"), Some(0));
        // HTTP-date form — intentionally unsupported (rare in practice
        // for LLM providers). Treated as "no hint".
        assert_eq!(
            parse_retry_after_seconds("Wed, 21 Oct 2025 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_seconds(""), None);
        assert_eq!(parse_retry_after_seconds("abc"), None);
    }

    #[test]
    fn upstream_rate_limit_hint_is_capped() {
        // An upstream that claims "retry in 2 hours" mostly means "we
        // gave up estimating" — capping at one hour keeps the header
        // useful for retries while not promising to come back in 24h.
        let err = GatewayError::UpstreamRateLimited {
            retry_after_secs: Some(7200),
        };
        assert_eq!(err.retry_after_secs(), Some(3600));
    }

    #[test]
    fn resolve_trace_id_accepts_caller_supplied_value() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "abc-12345".parse().unwrap());
        assert_eq!(resolve_trace_id(&h), "abc-12345");
    }

    #[test]
    fn resolve_trace_id_trims_whitespace() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "  spaced  ".parse().unwrap());
        assert_eq!(resolve_trace_id(&h), "spaced");
    }

    #[test]
    fn resolve_trace_id_mints_uuid_when_missing() {
        let h = HeaderMap::new();
        let id = resolve_trace_id(&h);
        // UUID v4 is 36 chars with 4 hyphens.
        assert_eq!(id.len(), 36, "expected v4 UUID, got {id}");
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn resolve_trace_id_rejects_too_long_header() {
        let mut h = HeaderMap::new();
        let long = "x".repeat(129);
        h.insert("x-trace-id", long.parse().unwrap());
        // Falls back to UUID when the header fails validation.
        let id = resolve_trace_id(&h);
        assert_ne!(id.len(), 129, "did not reject long header");
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn resolve_trace_id_rejects_empty_header() {
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", "   ".parse().unwrap());
        let id = resolve_trace_id(&h);
        assert_eq!(id.len(), 36, "blank header should fall back to UUID");
    }

    /// 128-char boundary: exactly 128 should pass, 129 should reject.
    #[test]
    fn resolve_trace_id_boundary() {
        let mut h128 = HeaderMap::new();
        let s128 = "a".repeat(128);
        h128.insert("x-trace-id", s128.parse().unwrap());
        assert_eq!(resolve_trace_id(&h128).len(), 128);

        let mut h129 = HeaderMap::new();
        let s129 = "a".repeat(129);
        h129.insert("x-trace-id", s129.parse().unwrap());
        assert_eq!(resolve_trace_id(&h129).len(), 36, "129 must fall back");
    }
}
