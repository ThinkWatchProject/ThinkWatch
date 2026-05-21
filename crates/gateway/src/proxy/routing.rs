//! Multi-route selection: strategy-driven weights + circuit-breaker
//! filter + per-model session affinity (none / provider / route).

use rand::RngExt;
use std::str::FromStr;
use uuid::Uuid;

use super::GatewayState;
use crate::health::{CircuitBreakerConfig, RouteHealth};
use crate::providers::traits::{ChatCompletionRequest, GatewayError};
use crate::router::{AffinityMode, RouteEntry};
use crate::strategy::{self, RoutingStrategy};

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
pub(super) async fn set_affinity(
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
    pub(super) model_id: &'a str,
    pub(super) user_id: Option<&'a str>,
    pub(super) strategy: RoutingStrategy,
    pub(super) affinity_mode: AffinityMode,
    pub(super) affinity_ttl_secs: u32,
    pub(super) latency_k: f64,
    pub(super) breaker: CircuitBreakerConfig,
    pub(super) state: &'a GatewayState,
}

pub(super) async fn build_selection_ctx<'a>(
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
#[derive(Clone)]
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
pub(crate) async fn finalize_health(state: &GatewayState, sel: &SelectionRecord, success: bool) {
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
pub(super) async fn select_route_with_failover<'a>(
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
pub(super) async fn select_route_for_stream<'a>(
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
