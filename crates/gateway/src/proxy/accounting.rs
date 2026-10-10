//! Post-flight accounting + token resolution for streaming responses.
//!
//! [`post_flight_account`] adds what a request used to its token-metric
//! sliding rules, its budget caps and the token cap of the route that
//! answered, from the token counts the upstream returned — or the
//! estimate, when it returned none — with cache reads and writes
//! weighted apart from plain input. Used from BOTH the
//! non-streaming branch (called inline after the upstream future
//! resolves) and the streaming branch (called from the post-invoke
//! pipeline after the SSE stream is drained).
//!
//! All errors are logged and swallowed — by the time we get here the
//! caller has already received their response, so refusing to account
//! isn't an option.

use std::sync::Arc;

use sqlx::PgPool;

use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::limits::{self, RateMetric, RequestLimits, sliding, weight};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn post_flight_account(
    db: PgPool,
    redis: fred::clients::Client,
    _dynamic_config: Arc<DynamicConfig>,
    weight_cache: weight::WeightCache,
    model: String,
    tokens: weight::TokenCounts,
    request_limits: &RequestLimits,
    // The route that answered, for its token cap. `None` when no route
    // did (an answer from the response cache).
    route: Option<&crate::route_caps::RouteCaps>,
    // Actor attribution for `budget.threshold_crossed` audit entries.
    // Without these the crossing log carries only `cap_id`, and
    // operators investigating a 100 %-cross had to time-join against
    // gateway_logs to find the user who pushed it over.
    actor_user_id: Option<String>,
    actor_user_email: Option<String>,
    actor_api_key_id: Option<String>,
    actor_ip_address: Option<String>,
    audit: think_watch_common::audit::AuditLogger,
) {
    let mult = weight_cache.get(&db, &model).await;
    let weighted = weight::weighted_tokens(&tokens, mult);
    if weighted <= 0 {
        return;
    }

    // The key's and its owner's day and month, counted whether or not
    // they have limits.
    record_usage_tokens(&redis, request_limits, weighted).await;

    // Token-metric sliding rules — the user's and the key's, the same
    // rules the pre-flight checked. Recorded whatever they come to: a
    // window this request overshoots refuses the next one. Post-flight
    // always runs fail-open because the response has already been
    // delivered: refusing to record the spend would just hide it from
    // analytics without recovering anything.
    if let Err(e) = sliding::record(
        &redis,
        &request_limits.rules,
        request_limits.owner,
        RateMetric::Tokens,
        weighted,
    )
    .await
    {
        tracing::warn!("token rate-limit accounting failed: {e}");
    }

    // The answering route's tokens-per-minute cap, counted the same way.
    if let Some(caps) = route {
        crate::route_caps::record_tokens(&redis, caps, weighted).await;
    }

    // Natural-period budget caps — the user's and the key's.
    if !request_limits.caps.is_empty() {
        let caps = &request_limits.caps;
        {
            match limits::budget::add_weighted_tokens(&redis, caps, weighted).await {
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

/// Add `weighted` tokens to the day and month usage counters of the
/// request's key and its owner (`limits::usage`). Fail-open: the counts
/// are for reading.
async fn record_usage_tokens(
    redis: &fred::clients::Client,
    request_limits: &RequestLimits,
    weighted: i64,
) {
    let Some(lineage) = request_limits.key_lineage else {
        return;
    };
    if let Err(e) = limits::usage::record_tokens(
        redis,
        request_limits.owner,
        lineage,
        weighted,
        chrono::Utc::now(),
    )
    .await
    {
        metrics::counter!("gateway_usage_count_fail_open_total").increment(1);
        tracing::warn!("usage token count failed: {e}");
    }
}
