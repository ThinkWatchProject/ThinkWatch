//! Multi-route selection: strategy-driven weights + circuit-breaker
//! filter + per-route capacity caps (`crate::route_caps`) + per-model
//! session affinity (none / provider / route) — and, above all of them,
//! the provider that holds a conversation's encrypted history
//! (`super::sealed`).

use rand::RngExt;
use std::str::FromStr;
use uuid::Uuid;

use super::GatewayState;
use crate::call_ctx::CallCtx;
use crate::error::GatewayError;
use crate::health::{CircuitBreakerConfig, RouteHealth};
use crate::route_caps::{self, Admission, CappedRoutes, RouteCaps};
use crate::router::{AffinityMode, RouteEntry};
use crate::strategy::{self, RoutingStrategy};

use super::retry::Budget;
use super::sealed::Pin;

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
    CircuitBreakerConfig::load(&state.dynamic_config).await
}

/// Strategy/affinity/breaker context resolved once per request and
/// reused through the failover loop.
pub(super) struct SelectionCtx<'a> {
    pub(super) model_id: &'a str,
    pub(super) user_id: Option<&'a str>,
    pub(super) strategy: RoutingStrategy,
    pub(super) affinity_mode: AffinityMode,
    pub(super) affinity_ttl_secs: u32,
    pub(super) latency_k: f64,
    pub(super) breaker: CircuitBreakerConfig,
    /// `security.rate_limit_fail_closed`, for the route caps.
    pub(super) fail_closed: bool,
    /// The provider holding the conversation's encrypted history, when
    /// the request carries some (see `super::sealed::decide`). Overrides
    /// the strategy and the affinity.
    pub(super) pin: Option<Pin>,
    pub(super) state: &'a GatewayState,
}

pub(super) async fn build_selection_ctx<'a>(
    state: &'a GatewayState,
    model_id: &'a str,
    user_id: Option<&'a str>,
    pin: Option<Pin>,
) -> SelectionCtx<'a> {
    let (strategy, affinity_mode, affinity_ttl_secs) =
        resolve_routing_config(state, model_id).await;
    let latency_k = state.dynamic_config.latency_strategy_k().await;
    let breaker = resolve_breaker_config(state).await;
    let fail_closed = state.dynamic_config.rate_limit_fail_closed().await;
    SelectionCtx {
        model_id,
        user_id,
        strategy,
        affinity_mode,
        affinity_ttl_secs,
        latency_k,
        breaker,
        fail_closed,
        pin,
        state,
    }
}

/// One selection attempt over the candidate set: snapshot health,
/// drop circuit-broken / already-tried (`tried`: provider ids) /
/// capped (`capped`: route ids) candidates, compute strategy weights,
/// pick.
async fn pick_with_strategy<'a>(
    group: &[&'a RouteEntry],
    tried: &[Uuid],
    capped: &[Uuid],
    ctx: &SelectionCtx<'_>,
) -> Option<&'a RouteEntry> {
    if group.is_empty() {
        return None;
    }
    let mut healths: Vec<RouteHealth> = Vec::with_capacity(group.len());
    for entry in group {
        let h = ctx.state.health.snapshot(entry.route_id, ctx.breaker).await;
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
        // A provider that alone can read the history is asked even with
        // its breaker open: there is nowhere else to go.
        let broken = h.state == tw_breaker::State::Open && !matches!(ctx.pin, Some(Pin::Only(_)));
        let excl = broken || tried.contains(&entry.provider_id) || capped.contains(&entry.route_id);
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
    pin_to_holder(group, &mut excluded, ctx.pin);

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

/// Narrow the candidates to the provider holding the conversation's
/// encrypted history: always for [`Pin::Only`], for [`Pin::First`] while
/// one of its routes is still eligible.
fn pin_to_holder(group: &[&RouteEntry], excluded: &mut [bool], pin: Option<Pin>) {
    let Some(pin) = pin else { return };
    let holder = pin.provider_id();
    let eligible = group
        .iter()
        .zip(excluded.iter())
        .any(|(e, x)| !*x && e.provider_id == holder);
    if matches!(pin, Pin::First(_)) && !eligible {
        return;
    }
    for (e, x) in group.iter().zip(excluded.iter_mut()) {
        if e.provider_id != holder {
            *x = true;
        }
    }
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
    /// How many times the request went out again after a transient
    /// failure before this route answered (see `super::retry`). Said on
    /// the log row as `"retries"`.
    pub retries: u32,
}

/// Record health for the picked route once the request finishes.
/// Best-effort; failures are logged.
pub(crate) async fn finalize_health(state: &GatewayState, sel: &SelectionRecord, success: bool) {
    let total_latency_ms = sel.started_at.elapsed().as_millis().min(u32::MAX as u128) as u32;
    // For health, we want the picked route's *own* time, not the
    // cumulative including prior failed attempts. Non-stream sets
    // this explicitly when it picks a winner; stream falls back to
    // total elapsed, which is the picked route's time: a stream that
    // went out again starts `started_at` at the attempt that answered.
    let health_latency_ms = sel.picked_latency_ms.unwrap_or(total_latency_ms);
    let breaker = resolve_breaker_config(state).await;
    let _ = state
        .health
        .record(sel.picked_route_id, health_latency_ms, !success, breaker)
        .await;
}

/// Did the upstream fail, as opposed to refusing this request?
///
/// Only a failure moves on to the next route and counts against the
/// route's circuit breaker:
///   * 5xx and 408 — the upstream had a problem answering.
///   * 429 — this upstream's quota; another route has its own.
///   * Timeouts, broken connections, an unreadable body — no usable
///     answer arrived.
///   * 401/403 (`UpstreamAuthError`) — the gateway's credential for this
///     route was refused. Nothing the caller did; another route carries
///     another credential.
///
/// Any other 4xx is the upstream refusing the request itself — a bad
/// parameter, a context too long. Every route would refuse it the same
/// way, so it goes straight back to the caller, and the route is not
/// held responsible: one caller's malformed requests would otherwise
/// walk every route and trip every breaker for the model. The desktop
/// gateway draws the same line.
///
/// Refusals by the gateway itself (`TransformError`, `PolicyBlocked`,
/// local limits) are not the upstream's doing either. Nor is an upstream
/// refusing a conversation's encrypted history (`SealedHistoryRefused`):
/// the history belongs to another provider, not to a broken route.
pub(crate) fn is_upstream_failure(err: &GatewayError) -> bool {
    fails(err.error_tag(), err.status_code())
}

/// [`is_upstream_failure`] from what a stream records of its error: the
/// error's tag and status (see `StreamOutcome::UpstreamError`). A stream
/// broken off in transit is tagged `transport` and counts as a failure.
pub(crate) fn fails(error_tag: &str, status: i64) -> bool {
    match error_tag {
        "ProviderHttpError" => status >= 500 || status == 408,
        "TransformError"
        | "PolicyBlocked"
        | "LocalRateLimited"
        | "SealedHistoryRefused"
        | "SealedHistoryUnavailable" => false,
        _ => true,
    }
}

/// What a buffered request ends with: the route that answered, the
/// answer, the ledger that restores it (the one its hop went out with),
/// and what to record against the route.
pub(super) type Answered<'a> = (
    &'a RouteEntry,
    crate::lifecycle::Completed,
    tw_guard::redact::replace::Ledger,
    SelectionRecord,
);

/// Pick a candidate the request may be sent to: one the strategy picks
/// among those not excluded, and under its caps — the pick is counted
/// against them. A pick at a cap is added to `capped` and another is
/// picked. `None` when no candidate is left.
async fn pick_admitted<'a>(
    group: &[&'a RouteEntry],
    tried: &[Uuid],
    capped: &mut CappedRoutes,
    ctx: &SelectionCtx<'_>,
) -> Result<Option<&'a RouteEntry>, GatewayError> {
    // Each pass excludes one more route, so this many passes see them all.
    for _ in 0..group.len() {
        let Some(entry) = pick_with_strategy(group, tried, capped.routes(), ctx).await else {
            return Ok(None);
        };
        match route_caps::admit(&ctx.state.redis, &RouteCaps::of(entry), ctx.fail_closed).await? {
            Admission::Admitted => return Ok(Some(entry)),
            Admission::Capped {
                label,
                retry_after_secs,
            } => {
                tracing::info!(
                    provider = %entry.provider_name,
                    route_id = %entry.route_id,
                    cap = %label,
                    retry_after_secs,
                    "Route at its cap, trying next"
                );
                capped.add(entry.route_id, label, retry_after_secs);
            }
        }
    }
    Ok(None)
}

/// Non-streaming selection + failover. All routes are peers (no
/// priority tier in v2): `pick_with_strategy` picks one healthy
/// candidate under its caps, the proxy calls it, and when the upstream
/// fails (see [`is_upstream_failure`]) tries another candidate from the
/// remaining set until exhausted. When every candidate left is at its
/// cap and none failed, the answer is the caps' 429.
///
/// When every candidate is exhausted and the last one failed for the
/// moment (see `super::retry`), the request waits as `budget` allows and
/// goes round again, to the providers whose last failure in this request
/// was transient — picked as before, so the strategy, the breaker, the
/// caps, the affinity and a sealed conversation's pin all apply. A model
/// with one route sends to it again.
pub(super) async fn select_route_with_failover<'a>(
    routes: &'a [RouteEntry],
    outbound: &super::generate::Outbound,
    call_ctx: &CallCtx,
    ctx: &SelectionCtx<'_>,
    caller_model: &str,
    budget: &mut Budget,
) -> Result<Answered<'a>, GatewayError> {
    let started_at = std::time::Instant::now();
    let candidates: Vec<&RouteEntry> = routes.iter().collect();

    let mut last_error: Option<GatewayError> = None;
    let mut tried: Vec<Uuid> = Vec::new();
    let mut capped = CappedRoutes::default();
    // The providers whose last failure in this request was transient, in
    // the order they failed: the ones a retry goes back to.
    let mut transient: Vec<Uuid> = Vec::new();

    loop {
        let retrying = budget.retries() > 0;
        let mut attempted = false;
        for _ in 0..candidates.len() {
            let Some(entry) = pick_admitted(&candidates, &tried, &mut capped, ctx).await? else {
                break;
            };
            tried.push(entry.provider_id);
            attempted = true;
            if retrying {
                super::retry::count(&entry.provider_name);
            }

            let upstream_model = entry.upstream_model.as_deref().unwrap_or(caller_model);

            // Per-attempt clock: record latency against this route as
            // *its* time, not "everything since the request started"
            // (which would double-count earlier failed attempts in
            // a failover chain and skew the latency strategy).
            let attempt_started_at = std::time::Instant::now();
            // A rejected dialect is retried inside `send`, on this same route.
            let result = match super::generate::send(
                entry,
                outbound,
                call_ctx,
                &ctx.state.db,
                upstream_model,
            )
            .await
            {
                Ok((resp, wire)) => {
                    super::generate::read_whole(resp, &wire, caller_model, outbound.input_estimate)
                        .await
                        .map(|answer| (answer, wire.ledger))
                }
                Err(e) => Err(e),
            };

            let attempt_latency_ms = attempt_started_at
                .elapsed()
                .as_millis()
                .min(u32::MAX as u128) as u32;

            match result {
                Ok((response, ledger)) => {
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
                        ledger,
                        SelectionRecord {
                            picked_route_id: entry.route_id,
                            started_at,
                            picked_latency_ms: Some(attempt_latency_ms),
                            retries: budget.retries(),
                        },
                    ));
                }
                Err(e) if is_upstream_failure(&e) => {
                    tracing::warn!(
                        provider = %entry.provider_name,
                        provider_id = %entry.provider_id,
                        error = %e,
                        "Route failed, trying next"
                    );
                    metrics::counter!(
                        "gateway_provider_fallback_total",
                        "from" => crate::metrics_labels::normalize_provider_label(&entry.provider_name),
                    )
                    .increment(1);
                    // Record the failed attempt in health so the
                    // breaker can trip mid-failover.
                    let _ = ctx
                        .state
                        .health
                        .record(entry.route_id, attempt_latency_ms, true, ctx.breaker)
                        .await;
                    transient.retain(|p| *p != entry.provider_id);
                    if super::retry::is_transient(&e) {
                        transient.push(entry.provider_id);
                    }
                    last_error = Some(e);
                    continue;
                }
                Err(e) => {
                    // The request itself was refused. Sibling routes would
                    // refuse it the same way, so it goes back to the caller.
                    // An upstream that answered with a refusal is working:
                    // it counts as a success for the route, as it does on
                    // the desktop.
                    if matches!(
                        e,
                        GatewayError::ProviderHttpError { .. }
                            | GatewayError::SealedHistoryRefused { .. }
                    ) {
                        let _ = ctx
                            .state
                            .health
                            .record(entry.route_id, attempt_latency_ms, false, ctx.breaker)
                            .await;
                    }
                    return Err(e);
                }
            }
        }

        // Every candidate is exhausted. Go round again only when this
        // round sent something, its last failure was transient, and the
        // wait fits before the deadline.
        let wait = match &last_error {
            Some(e) if attempted && !transient.is_empty() => budget.wait_after(e),
            _ => None,
        };
        let Some(wait) = wait else { break };
        tracing::info!(
            model = ctx.model_id,
            retry = budget.retries() + 1,
            wait_ms = wait.as_millis() as u64,
            error = ?last_error.as_ref().map(GatewayError::error_tag),
            "Every route failed for the moment, sending again after a wait"
        );
        tokio::time::sleep(wait).await;
        budget.sent();
        // Only the providers that failed for the moment are asked again.
        // Caps are counted afresh: a window full then may have room now.
        tried = candidates
            .iter()
            .map(|e| e.provider_id)
            .filter(|p| !transient.contains(p))
            .collect();
        capped = CappedRoutes::default();
    }

    // An upstream that failed is the answer's cause even when the rest
    // were capped; with no failure, the caps are.
    Err(last_error.or_else(|| capped.refusal()).unwrap_or_else(|| {
        GatewayError::ProviderError(format!("All routes failed for model: {}", ctx.model_id))
    }))
}

/// Streaming variant: pick via strategy + health filter + caps, but
/// don't call the provider — caller wires up the SSE stream and once the
/// first chunk lands a retry is no longer possible. Returns the
/// chosen entry plus a `SelectionRecord` so the streaming caller can
/// record health on stream completion. Every candidate at its cap: the
/// caps' 429.
pub(super) async fn select_route_for_stream<'a>(
    routes: &'a [RouteEntry],
    ctx: &SelectionCtx<'_>,
) -> Result<(&'a RouteEntry, SelectionRecord), GatewayError> {
    let started_at = std::time::Instant::now();
    let candidates: Vec<&RouteEntry> = routes.iter().collect();
    let mut capped = CappedRoutes::default();

    if let Some(entry) = pick_admitted(&candidates, &[], &mut capped, ctx).await? {
        return Ok((
            entry,
            SelectionRecord {
                picked_route_id: entry.route_id,
                started_at,
                // Streams don't loop — finalize_health falls
                // back to elapsed-from-started_at, which IS the
                // picked route's wall time for the streaming case.
                picked_latency_ms: None,
                retries: 0,
            },
        ));
    }

    Err(capped.refusal().unwrap_or_else(|| {
        GatewayError::ProviderError(format!("No provider found for model: {}", ctx.model_id))
    }))
}

/// What a stream needs to go out again to its route after a transient
/// failure before any of the answer reached the client (see
/// `super::retry`). A stream is sent to the one route it picked, so that
/// is where it goes again, under the pin and affinity it was picked with.
pub(crate) struct Resend {
    state: GatewayState,
    route_id: Uuid,
    provider: String,
    caps: RouteCaps,
    breaker: CircuitBreakerConfig,
    fail_closed: bool,
    /// The route's provider alone can read the conversation's encrypted
    /// history: asked again with its breaker open, as it was picked.
    pinned_only: bool,
    pub(crate) budget: Budget,
}

impl Resend {
    pub(super) fn new(entry: &RouteEntry, ctx: &SelectionCtx<'_>, budget: Budget) -> Self {
        Self {
            state: ctx.state.clone(),
            route_id: entry.route_id,
            provider: entry.provider_name.clone(),
            caps: RouteCaps::of(entry),
            breaker: ctx.breaker,
            fail_closed: ctx.fail_closed,
            pinned_only: matches!(ctx.pin, Some(Pin::Only(p)) if p == entry.provider_id),
            budget,
        }
    }

    /// Before the wait: may the route be sent to again? Not while its
    /// breaker is open, nor at one of its caps — the pick a failover makes;
    /// the retry is counted against the caps now. When it may, the attempt
    /// that failed (`failed_ms` long) counts against the route, as a failed
    /// attempt in a failover does, and the retry is counted. When it may
    /// not, the failure is the answer and the stream's tail records it.
    pub(crate) async fn admit(&mut self, failed_ms: u32) -> bool {
        let health = self
            .state
            .health
            .snapshot(self.route_id, self.breaker)
            .await;
        if health.state == tw_breaker::State::Open && !self.pinned_only {
            return false;
        }
        match route_caps::admit(&self.state.redis, &self.caps, self.fail_closed).await {
            Ok(Admission::Admitted) => {}
            Ok(Admission::Capped { .. }) | Err(_) => return false,
        }
        let _ = self
            .state
            .health
            .record(self.route_id, failed_ms, true, self.breaker)
            .await;
        self.budget.sent();
        super::retry::count(&self.provider);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn entry(provider_id: Uuid) -> RouteEntry {
        RouteEntry {
            upstream: Arc::new(crate::proxy::transport::Upstream::new(
                "https://example.invalid",
                Vec::new(),
                crate::proxy::transport::Shape::Standard,
                "p",
            )),
            provider_id,
            route_id: Uuid::new_v4(),
            provider_name: "p".into(),
            upstream_model: None,
            weight: 100,
            label: None,
            rpm_cap: None,
            tpm_cap: None,
            protocol: crate::protocol::UpstreamProtocol::OpenAiResponses,
            alternates: Vec::new(),
        }
    }

    /// Which of `a`, `b`, `a` (a second route of the holder) are left.
    fn left(pin: Option<Pin>, a: Uuid, excluded: [bool; 3]) -> [bool; 3] {
        let routes = [entry(a), entry(Uuid::new_v4()), entry(a)];
        let group: Vec<&RouteEntry> = routes.iter().collect();
        let mut excluded = excluded;
        pin_to_holder(&group, &mut excluded, pin);
        excluded.map(|x| !x)
    }

    #[test]
    fn a_pin_narrows_the_candidates_to_the_holders_routes() {
        let a = Uuid::new_v4();
        let none = [false; 3];
        assert_eq!(left(None, a, none), [true, true, true]);
        assert_eq!(left(Some(Pin::Only(a)), a, none), [true, false, true]);
        assert_eq!(left(Some(Pin::First(a)), a, none), [true, false, true]);
    }

    #[test]
    fn only_sealed_reasoning_falls_over_once_the_holder_is_out() {
        let a = Uuid::new_v4();
        // The holder tried (or capped, or broken): reasoning moves on…
        let out = [true, false, true];
        assert_eq!(left(Some(Pin::First(a)), a, out), [false, true, false]);
        // …anything else sealed has nowhere to go.
        assert_eq!(left(Some(Pin::Only(a)), a, out), [false, false, false]);
    }

    #[test]
    fn sealed_refusals_and_refusing_up_front_do_not_count_against_the_route() {
        assert!(!fails("SealedHistoryRefused", 400));
        assert!(!fails("SealedHistoryUnavailable", 409));
        assert!(fails("ProviderHttpError", 500));
    }
}
