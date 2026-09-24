//! Post-flight accounting + token resolution for streaming responses.
//!
//! [`post_flight_account`] runs the token-metric sliding rules and
//! budget caps against the token counts the upstream returned — or the
//! estimate, when it returned none — with cache reads and writes
//! weighted apart from plain input. Used from BOTH the non-streaming branch (called
//! inline after the upstream future resolves) and the streaming branch
//! (called from the post-invoke pipeline after the SSE stream is
//! drained).
//!
//! All errors are logged and swallowed — by the time we get here the
//! caller has already received their response, so refusing to account
//! isn't an option.

use std::sync::Arc;

use sqlx::PgPool;

use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::limits::{self, BudgetCap, RateMetric, sliding, weight};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn post_flight_account(
    db: PgPool,
    redis: fred::clients::Client,
    _dynamic_config: Arc<DynamicConfig>,
    weight_cache: weight::WeightCache,
    model: String,
    tokens: weight::TokenCounts,
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
    let weighted = weight::weighted_tokens(&tokens, mult);
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
