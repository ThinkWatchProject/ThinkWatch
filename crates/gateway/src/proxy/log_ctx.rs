//! Per-handler error-logging context and the three audit-emit
//! functions every AI surface routes through. Built once near the top
//! of each AI proxy handler and consumed by `return Err(ctx.emit(...))`
//! at early-return points so an error never leaves the gateway without
//! a matching `gateway_logs` row. The success path uses
//! [`emit_gateway_log`] directly because it has tokens / cost info that
//! the error path doesn't.

use rust_decimal::Decimal;

use super::GatewayRequestIdentity;
use super::body_capture::BodyCapture;
use super::gateway_error_status;
use crate::error::GatewayError;

/// Per-handler error-logging context.
///
/// Fields are owned (cheap clones at request entry) because the
/// handler later mutates `request` and a borrow of `request.model`
/// would block that with a partial-borrow error. The audit handle is
/// borrowed because it's already an `Arc` internally.
pub(super) struct LogCtx<'a> {
    pub(super) audit: &'a think_watch_common::audit::AuditLogger,
    pub(super) trace_id: String,
    pub(super) session_id: Option<String>,
    pub(super) user_id: Option<String>,
    pub(super) user_email: Option<String>,
    pub(super) api_key_id: Option<String>,
    pub(super) api_key_lineage_id: Option<String>,
    pub(super) ip_address: Option<String>,
    /// May be "(unknown)" when the failure happens before the model
    /// has been resolved (e.g. transform errors on malformed bodies).
    pub(super) model: String,
    pub(super) started: std::time::Instant,
}

impl<'a> LogCtx<'a> {
    /// Build the audit context from the identity + a model name. Used
    /// at handler entry (and rebuilt with a refined model once the
    /// mapped name is known, where applicable).
    pub(super) fn new(
        audit: &'a think_watch_common::audit::AuditLogger,
        identity: &GatewayRequestIdentity,
        trace_id: &str,
        session_id: Option<&str>,
        model: impl Into<String>,
        started: std::time::Instant,
    ) -> Self {
        Self {
            audit,
            trace_id: trace_id.to_string(),
            session_id: session_id.map(|s| s.to_string()),
            user_id: identity.user_id.clone(),
            user_email: identity.user_email.clone(),
            api_key_id: identity.api_key_id.clone(),
            api_key_lineage_id: identity.api_key_lineage_id.clone(),
            ip_address: identity.ip_address.clone(),
            model: model.into(),
            started,
        }
    }

    /// Emit the error row and return the error unchanged so call sites
    /// stay one-line:  `return Err(ctx.emit(GatewayError::...));`
    pub(super) fn emit(&self, err: GatewayError) -> GatewayError {
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

/// Emit a single `gateway_logs` row for a failed request. The detail
/// blob mirrors the success-path shape but also carries `error_type`
/// and `error_message` so operators can drill down without joining
/// against a separate error-log stream.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gateway_error_log(
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
pub(crate) fn emit_gateway_log_with_extra(
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
pub(super) fn emit_gateway_log(
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
