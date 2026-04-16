use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use rand::RngExt;
use sqlx::PgPool;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::cache::ResponseCache;
use crate::content_filter::{Action, ContentFilter};
use crate::cost_tracker::CostTracker;
use crate::metadata::RequestMetadata;
use crate::model_mapping::ModelMapper;
use crate::pii_redactor::{PiiRedactor, PiiStreamRestorer};
use crate::providers::traits::{ChatCompletionRequest, GatewayError};
use crate::quota::QuotaManager;
use crate::rate_limiter::RateLimiter;
use crate::router::{ModelRouter, RouteEntry};
use crate::streaming::stream_to_sse_with_restorer;
use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::limits::{
    self, BudgetCap, BudgetSubject, RateLimitRule, RateLimitSubject, RateMetric, Surface,
    SurfaceConstraints, sliding, weight,
};

/// Shared application state for the gateway proxy handlers.
#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<ModelRouter>,
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
    /// model multipliers; raw rules go through a separate cache later.
    pub db: PgPool,
    /// Redis client used by the bucketed sliding-window engine and the
    /// natural-period budget counters. Same connection used by `quota`,
    /// `cache`, and the rest of the gateway.
    pub redis: fred::clients::Client,
    /// LRU cache mapping `model_id → (input_multiplier, output_multiplier)`.
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
    pub allowed_models: Option<Vec<String>>,
    /// Merged-across-roles inline limits (most restrictive per
    /// surface+metric+window / surface+period). Computed once by the
    /// auth middleware via `rbac::compute_user_surface_constraints`
    /// and consumed directly here — no side-table lookups on the
    /// hot path.
    pub surface_constraints: SurfaceConstraints,
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
    user_id: Option<String>,
    api_key_id: Option<String>,
    /// May be "(unknown)" when the failure happens before the model
    /// has been resolved (e.g. transform errors on malformed bodies).
    model: String,
    started: std::time::Instant,
}

impl LogCtx<'_> {
    /// Emit the error row and return the error unchanged so call sites
    /// stay one-line:  `return Err(ctx.emit(GatewayError::...));`
    fn emit(&self, err: GatewayError) -> GatewayError {
        emit_gateway_error_log(
            self.audit,
            &self.trace_id,
            self.user_id.as_deref(),
            self.api_key_id.as_deref(),
            &self.model,
            self.started.elapsed().as_millis() as i64,
            &err,
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

/// Map a `GatewayError` to the HTTP status we'll actually return so
/// the error-path `gateway_logs` row carries the same status code the
/// client saw. Keep in sync with `GatewayErrorResponse::into_response`
/// below — drift there would make traces misleading.
fn gateway_error_status(err: &GatewayError) -> i64 {
    match err {
        GatewayError::ProviderError(_) => 502,
        GatewayError::TransformError(_) => 400,
        GatewayError::NetworkError(_) => 502,
        GatewayError::UpstreamRateLimited | GatewayError::LocalRateLimited(_) => 429,
        GatewayError::UpstreamAuthError => 401,
    }
}

/// Emit a single `gateway_logs` row for a failed request. The detail
/// blob mirrors the success-path shape but also carries `error_type`
/// and `error_message` so operators can drill down without joining
/// against a separate error-log stream.
#[allow(clippy::too_many_arguments)]
fn emit_gateway_error_log(
    audit: &think_watch_common::audit::AuditLogger,
    trace_id: &str,
    user_id: Option<&str>,
    api_key_id: Option<&str>,
    model_id: &str,
    latency_ms: i64,
    err: &GatewayError,
) {
    let status = gateway_error_status(err);
    let detail = serde_json::json!({
        "model_id": model_id,
        "input_tokens": 0i64,
        "output_tokens": 0i64,
        "cost_usd": 0.0,
        "latency_ms": latency_ms,
        "status_code": status,
        "error_type": format!("{err:?}").split('(').next().unwrap_or("Error"),
        "error_message": err.to_string(),
    });
    // Same `chat.completion` action as the success path — flush_gateway
    // drops the action when it writes ChGatewayRow, so the trace
    // endpoint distinguishes errors via `status_code` (>= 400) instead.
    // Detail carries error_type + error_message for the drill-down.
    let mut entry = think_watch_common::audit::AuditEntry::gateway("chat.completion")
        .trace_id(trace_id.to_string())
        .detail(detail);
    if let Some(uid) = user_id
        && let Ok(u) = uuid::Uuid::parse_str(uid)
    {
        entry = entry.user_id(u);
    }
    if let Some(kid) = api_key_id
        && let Ok(u) = uuid::Uuid::parse_str(kid)
    {
        entry = entry.api_key_id(u);
    }
    audit.log(entry);
}

#[allow(clippy::too_many_arguments)]
fn emit_gateway_log(
    audit: &think_watch_common::audit::AuditLogger,
    trace_id: &str,
    user_id: Option<&str>,
    api_key_id: Option<&str>,
    model_id: &str,
    provider: Option<&str>,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost_usd: f64,
    latency_ms: i64,
    status_code: i64,
) {
    let mut entry = think_watch_common::audit::AuditEntry::gateway("chat.completion")
        .trace_id(trace_id.to_string())
        .detail(serde_json::json!({
            "model_id": model_id,
            "provider": provider,
            "input_tokens": prompt_tokens as i64,
            "output_tokens": completion_tokens as i64,
            "cost_usd": cost_usd,
            "latency_ms": latency_ms,
            "status_code": status_code,
        }));
    if let Some(uid) = user_id
        && let Ok(u) = uuid::Uuid::parse_str(uid)
    {
        entry = entry.user_id(u);
    }
    if let Some(kid) = api_key_id
        && let Ok(u) = uuid::Uuid::parse_str(kid)
    {
        entry = entry.api_key_id(u);
    }
    audit.log(entry);
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
                    // Emit one `budget.threshold_crossed` audit entry
                    // per crossing so any forwarder subscribed to
                    // `audit` log_type picks them up alongside key /
                    // role events.
                    for crossing in &crossings {
                        audit.log(
                            think_watch_common::audit::AuditEntry::new("budget.threshold_crossed")
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
    let _provider_id = state.router.provider_id_for(model);
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
// Multi-route selection: failover + weighted random + session affinity
// ---------------------------------------------------------------------------

/// Check Redis for session affinity: `affinity:{user_id}:{model_id}`.
/// Returns the cached provider_id if present and still in the given
/// route entries, so the caller can skip weighted random selection.
async fn check_affinity(
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
    entries: &[&RouteEntry],
) -> Option<Uuid> {
    use fred::interfaces::KeysInterface;
    let uid = user_id?;
    let key = format!("affinity:{uid}:{model}");
    let val: Option<String> = redis.get(&key).await.ok().flatten();
    let pid = val.and_then(|s| Uuid::parse_str(&s).ok())?;
    // Only use affinity if the provider is actually in the current group
    if entries.iter().any(|e| e.provider_id == pid) {
        Some(pid)
    } else {
        None
    }
}

/// Store session affinity in Redis with a 5-minute TTL.
async fn set_affinity(
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
    provider_id: Uuid,
) {
    use fred::interfaces::KeysInterface;
    let Some(uid) = user_id else { return };
    let key = format!("affinity:{uid}:{model}");
    let _: Result<(), _> = redis
        .set::<(), _, _>(
            &key,
            provider_id.to_string(),
            Some(fred::types::Expiration::EX(300)),
            None,
            false,
        )
        .await;
}

/// Pick one route from a slice of entries by weighted random selection.
/// If `affinity_provider` matches one in the slice, use that instead.
fn pick_weighted<'a>(
    entries: &[&'a RouteEntry],
    affinity_provider: Option<Uuid>,
) -> Option<&'a RouteEntry> {
    if entries.is_empty() {
        return None;
    }
    // Affinity override
    if let Some(pid) = affinity_provider
        && let Some(entry) = entries.iter().find(|e| e.provider_id == pid)
    {
        return Some(entry);
    }
    // Weighted random
    let total_weight: u32 = entries.iter().map(|e| e.weight).sum();
    if total_weight == 0 {
        return entries.first().copied();
    }
    let mut rng = rand::rng();
    let mut pick = rng.random_range(0..total_weight);
    for entry in entries {
        if pick < entry.weight {
            return Some(entry);
        }
        pick -= entry.weight;
    }
    entries.last().copied()
}

/// Returns true if the error is retryable (network, 502, 503, 429).
fn is_retryable(err: &GatewayError) -> bool {
    matches!(
        err,
        GatewayError::NetworkError(_)
            | GatewayError::ProviderError(_)
            | GatewayError::UpstreamRateLimited
    )
}

/// Group route entries by priority and attempt each group in order.
/// Within a priority group, use weighted random + affinity, then
/// failover to other members of the group before advancing.
///
/// Returns the selected provider and the upstream model name to use.
/// On success sets session affinity.
async fn select_route_with_failover<'a>(
    routes: &'a [RouteEntry],
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
    request: &ChatCompletionRequest,
    is_stream: bool,
) -> Result<
    (
        &'a RouteEntry,
        crate::providers::traits::ChatCompletionResponse,
    ),
    GatewayError,
> {
    // Group by priority
    let mut priority_groups: Vec<(u32, Vec<&RouteEntry>)> = Vec::new();
    for entry in routes {
        if let Some(group) = priority_groups.last_mut()
            && group.0 == entry.priority
        {
            group.1.push(entry);
            continue;
        }
        priority_groups.push((entry.priority, vec![entry]));
    }

    let mut last_error: Option<GatewayError> = None;

    for (_prio, group) in &priority_groups {
        let affinity = check_affinity(redis, user_id, model, group).await;
        let mut tried: Vec<Uuid> = Vec::new();

        // Try up to group.len() times within this priority group
        for _ in 0..group.len() {
            // Filter out already-tried providers
            let remaining: Vec<&RouteEntry> = group
                .iter()
                .filter(|e| !tried.contains(&e.provider_id))
                .copied()
                .collect();
            if remaining.is_empty() {
                break;
            }

            let affinity_for_pick = if tried.is_empty() { affinity } else { None };
            let Some(entry) = pick_weighted(&remaining, affinity_for_pick) else {
                break;
            };
            tried.push(entry.provider_id);

            // Build request with upstream_model if set
            let mut req = request.clone();
            if let Some(ref upstream) = entry.upstream_model {
                req.model = upstream.clone();
            }

            let result = if is_stream {
                // For streaming: we can't retry once streaming starts,
                // so do a non-streaming probe. Actually for streaming we
                // just return the entry and let the caller set up the stream.
                // We return a dummy response — the caller will use the entry directly.
                return Ok((
                    entry,
                    crate::providers::traits::ChatCompletionResponse {
                        id: String::new(),
                        object: "chat.completion".into(),
                        created: 0,
                        model: model.to_string(),
                        choices: vec![],
                        usage: None,
                    },
                ));
            } else {
                entry.provider.chat_completion_boxed(req).await
            };

            match result {
                Ok(response) => {
                    set_affinity(redis, user_id, model, entry.provider_id).await;
                    return Ok((entry, response));
                }
                Err(e) if is_retryable(&e) => {
                    tracing::warn!(
                        provider = %entry.provider.name(),
                        provider_id = %entry.provider_id,
                        error = %e,
                        "Route failed, trying next"
                    );
                    last_error = Some(e);
                    continue;
                }
                Err(e) => {
                    // Non-retryable error — fail immediately
                    return Err(e);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::ProviderError(format!("All routes failed for model: {model}"))
    }))
}

/// Streaming variant: selects a route with affinity + failover but does
/// NOT actually call the provider. Returns the chosen entry so the caller
/// can set up the stream. For streaming, retry is only possible before
/// the first chunk, so we just pick the best route.
async fn select_route_for_stream<'a>(
    routes: &'a [RouteEntry],
    redis: &fred::clients::Client,
    user_id: Option<&str>,
    model: &str,
) -> Result<&'a RouteEntry, GatewayError> {
    // Group by priority, pick from the first group with affinity support
    let mut priority_groups: Vec<(u32, Vec<&RouteEntry>)> = Vec::new();
    for entry in routes {
        if let Some(group) = priority_groups.last_mut()
            && group.0 == entry.priority
        {
            group.1.push(entry);
            continue;
        }
        priority_groups.push((entry.priority, vec![entry]));
    }

    if let Some((_prio, group)) = priority_groups.first() {
        let affinity = check_affinity(redis, user_id, model, group).await;
        if let Some(entry) = pick_weighted(group, affinity) {
            return Ok(entry);
        }
    }

    Err(GatewayError::ProviderError(format!(
        "No provider found for model: {model}"
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
    let request_started_at = std::time::Instant::now();
    let ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        user_id: identity.user_id.clone(),
        api_key_id: identity.api_key_id.clone(),
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
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {m}");
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => {
                tracing::warn!("Content filter warning (request allowed): {m}");
            }
            Action::Log => {
                tracing::info!("Content filter log: {m}");
            }
        }
    }

    // 5. Attach caller identity for custom header template resolution
    request.caller_user_id = identity.user_id.clone();
    request.caller_user_email = identity.user_email.clone();

    // 6. PII redaction — redact user messages before sending upstream
    let pii_redactor = state.pii_redactor.load();
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
    if let Some(cached) = state.cache.get(&request).await {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
        tracing::debug!(model = %request.model, stream = is_stream, "Cache HIT");
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
                metadata.request_id.parse().unwrap(),
            );
            return Ok(response);
        }
        let mut response = Json(&cached).into_response();
        response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("HIT"));
        response.headers_mut().insert(
            "X-Metadata-Request-Id",
            metadata.request_id.parse().unwrap(),
        );
        return Ok(response);
    }
    metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);

    // Route to provider — multi-route failover
    let original_model = request.model.clone();
    let routes = state.router.route(&request.model).ok_or_else(|| {
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
        let entry = select_route_for_stream(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &original_model,
        )
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
            entry.provider_id,
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
        let user_id_for_done = identity.user_id.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let model_for_log = original_model.clone();
        let started = request_started_at;
        // Clone request for cache write — the original is moved into the provider.
        let request_for_cache = request.clone();
        let cache_for_done = state.cache.clone();
        let stream = entry.provider.stream_chat_completion(request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                let (pt, ct) = result
                    .usage
                    .as_ref()
                    .map(|u| (u.prompt_tokens, u.completion_tokens))
                    .unwrap_or((0, 0));
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct);
                emit_gateway_log(
                    &audit_for_done,
                    &trace_id_for_done,
                    user_id_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    &model_for_log,
                    None,
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    200,
                );

                // Assemble and cache the complete response when the
                // stream ran to natural completion (partial streams
                // from client disconnects are NOT cached).
                if result.natural_completion
                    && let Some(assembled) =
                        crate::streaming::assemble_response(&result.chunks, result.usage.clone())
                {
                    cache_for_done
                        .set(&request_for_cache, &assembled, None)
                        .await;
                }

                let Some(u) = result.usage else {
                    return;
                };
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model,
                    u.prompt_tokens,
                    u.completion_tokens,
                    request_rules_for_done,
                    budget_caps.clone(),
                    audit_for_done,
                )
                .await;
            })
        };
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        Ok(stream_to_sse_with_restorer(stream, on_done, stream_restorer).into_response())
    } else {
        // Non-streaming: full failover with retry across priority groups
        let (_entry, mut response) = select_route_with_failover(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &original_model,
            &request,
            false,
        )
        .await
        .map_err(|e| {
            emit_gateway_error_log(
                &state.audit,
                &metadata.request_id,
                identity.user_id.as_deref(),
                identity.api_key_id.as_deref(),
                &original_model,
                request_started_at.elapsed().as_millis() as i64,
                &e,
            );
            GatewayErrorResponse::from(e)
        })?;

        // Restore original model name in response (don't leak upstream_model)
        response.model = original_model.clone();

        // 8a. Restore PII in the response
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // 8b. Consume quota based on actual token usage
        if let Some(ref usage) = response.usage {
            let total = usage.total_tokens;
            if let Err(e) = state.quota.consume(&quota_key, total).await {
                tracing::warn!("Failed to consume quota: {e}");
            }
        }

        // 8b.1. Post-flight accounting against the limits engine.
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
                state.audit.clone(),
            )
            .await;
        }

        // 8d. Cache the response
        state.cache.set(&request, &response, None).await;

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
        let cost_usd =
            state
                .cost_tracker
                .calculate_cost(&original_model, prompt_tokens, completion_tokens);
        emit_gateway_log(
            &state.audit,
            &metadata.request_id,
            identity.user_id.as_deref(),
            identity.api_key_id.as_deref(),
            &original_model,
            None,
            prompt_tokens,
            completion_tokens,
            cost_usd,
            request_started_at.elapsed().as_millis() as i64,
            200,
        );

        let mut http_response = Json(&response).into_response();
        http_response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("MISS"));
        http_response.headers_mut().insert(
            "X-Metadata-Request-Id",
            metadata.request_id.parse().unwrap(),
        );
        Ok(http_response)
    }
}

/// GET /v1/models
///
/// Returns the list of available models in OpenAI-compatible format.
pub async fn list_models_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let models = state.router.list_models();

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
    let request_started_at = std::time::Instant::now();

    // Build LogCtx with model="(unknown)" up-front so a missing-model
    // body still emits an error row. We rebuild it once we know the
    // real model so subsequent emits attribute correctly.
    let early_ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        user_id: identity.user_id.clone(),
        api_key_id: identity.api_key_id.clone(),
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
        user_id: identity.user_id.clone(),
        api_key_id: identity.api_key_id.clone(),
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
    let routes = state.router.route(&mapped_model).ok_or_else(|| {
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
                });
            }
        }
    }

    // PII redaction
    let pii_redactor = state.pii_redactor.load();
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
    };

    if is_stream {
        let entry = select_route_for_stream(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
        )
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
            entry.provider_id,
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
        let user_id_for_done = identity.user_id.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let model_for_log = mapped_model.clone();
        let started = request_started_at;
        let stream = entry.provider.stream_chat_completion(stream_request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                let (pt, ct) = result
                    .usage
                    .as_ref()
                    .map(|u| (u.prompt_tokens, u.completion_tokens))
                    .unwrap_or((0, 0));
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct);
                emit_gateway_log(
                    &audit_for_done,
                    &trace_id_for_done,
                    user_id_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    &model_for_log,
                    None,
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    200,
                );
                let Some(u) = result.usage else {
                    return;
                };
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model_for_done,
                    u.prompt_tokens,
                    u.completion_tokens,
                    request_rules_for_done,
                    budget_caps.clone(),
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
        let (_entry, mut response) = select_route_with_failover(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            &request,
            false,
        )
        .await
        .map_err(|e| {
            emit_gateway_error_log(
                &state.audit,
                &trace_id,
                identity.user_id.as_deref(),
                identity.api_key_id.as_deref(),
                &mapped_model,
                request_started_at.elapsed().as_millis() as i64,
                &e,
            );
            GatewayErrorResponse::from(e)
        })?;

        // Restore original model name
        response.model = mapped_model.clone();

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
        let cost = state.cost_tracker.calculate_cost(&mapped_model, pt, ct);
        emit_gateway_log(
            &state.audit,
            &trace_id,
            identity.user_id.as_deref(),
            identity.api_key_id.as_deref(),
            &mapped_model,
            None,
            pt,
            ct,
            cost,
            request_started_at.elapsed().as_millis() as i64,
            200,
        );

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
    let request_started_at = std::time::Instant::now();

    let early_ctx = LogCtx {
        audit: &state.audit,
        trace_id: trace_id.clone(),
        user_id: identity.user_id.clone(),
        api_key_id: identity.api_key_id.clone(),
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
        user_id: identity.user_id.clone(),
        api_key_id: identity.api_key_id.clone(),
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
        });
    }

    match body.get("input") {
        Some(serde_json::Value::String(s)) => {
            messages.push(crate::providers::traits::ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(s.clone()),
            });
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                // Each item can be a message object or a string
                if let Some(s) = item.as_str() {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::String(s.to_string()),
                    });
                } else if let (Some(role), Some(content)) = (
                    item.get("role").and_then(|v| v.as_str()),
                    item.get("content"),
                ) {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: role.to_string(),
                        content: content.clone(),
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
    };

    // Route to provider — multi-route failover
    let routes = state.router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;

    if is_stream {
        let entry = select_route_for_stream(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
        )
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
            entry.provider_id,
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
        let user_id_for_done = identity.user_id.clone();
        let api_key_id_for_done = identity.api_key_id.clone();
        let model_for_log = mapped_model.clone();
        let started = request_started_at;
        let stream = entry.provider.stream_chat_completion(stream_request);
        let on_done = move |result: crate::streaming::StreamResult|
            -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                let (pt, ct) = result
                    .usage
                    .as_ref()
                    .map(|u| (u.prompt_tokens, u.completion_tokens))
                    .unwrap_or((0, 0));
                let cost = cost_tracker.calculate_cost(&model_for_log, pt, ct);
                emit_gateway_log(
                    &audit_for_done,
                    &trace_id_for_done,
                    user_id_for_done.as_deref(),
                    api_key_id_for_done.as_deref(),
                    &model_for_log,
                    None,
                    pt,
                    ct,
                    cost,
                    started.elapsed().as_millis() as i64,
                    200,
                );
                let Some(u) = result.usage else {
                    return;
                };
                post_flight_account(
                    db,
                    redis,
                    dynamic_config,
                    weight_cache,
                    model_for_done,
                    u.prompt_tokens,
                    u.completion_tokens,
                    request_rules_for_done,
                    budget_caps.clone(),
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
        let (_entry, mut response) = select_route_with_failover(
            routes,
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            &request,
            false,
        )
        .await
        .map_err(|e| {
            emit_gateway_error_log(
                &state.audit,
                &trace_id,
                identity.user_id.as_deref(),
                identity.api_key_id.as_deref(),
                &mapped_model,
                request_started_at.elapsed().as_millis() as i64,
                &e,
            );
            GatewayErrorResponse::from(e)
        })?;

        // Restore original model name
        response.model = mapped_model.clone();

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
                state.audit.clone(),
            )
            .await;
        }

        let (pt, ct) = response
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let cost = state.cost_tracker.calculate_cost(&mapped_model, pt, ct);
        emit_gateway_log(
            &state.audit,
            &trace_id,
            identity.user_id.as_deref(),
            identity.api_key_id.as_deref(),
            &mapped_model,
            None,
            pt,
            ct,
            cost,
            request_started_at.elapsed().as_millis() as i64,
            200,
        );

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
        use axum::http::StatusCode;

        let (status, error_type) = match &self.0 {
            GatewayError::ProviderError(_) => (StatusCode::BAD_GATEWAY, "provider_error"),
            GatewayError::TransformError(_) => (StatusCode::BAD_REQUEST, "transform_error"),
            GatewayError::NetworkError(_) => (StatusCode::BAD_GATEWAY, "network_error"),
            GatewayError::UpstreamRateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            GatewayError::LocalRateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            GatewayError::UpstreamAuthError => (StatusCode::UNAUTHORIZED, "auth_error"),
        };

        let body = serde_json::json!({
            "error": {
                "message": self.0.to_string(),
                "type": error_type,
            }
        });

        (status, Json(body)).into_response()
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
            (GatewayError::TransformError("x".into()), 400),
            (GatewayError::NetworkError("x".into()), 502),
            (GatewayError::UpstreamRateLimited, 429),
            (GatewayError::LocalRateLimited("rule".into()), 429),
            (GatewayError::UpstreamAuthError, 401),
        ] {
            assert_eq!(
                gateway_error_status(&err),
                expected,
                "status mismatch for {err:?}"
            );
        }
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
