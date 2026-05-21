//! Post-flight accounting + token resolution for streaming responses.
//!
//! [`post_flight_account`] runs the token-metric sliding rules and
//! budget caps against the real prompt/completion token counts the
//! upstream returned. Used from BOTH the non-streaming branch (called
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
pub(crate) fn stream_usage_or_estimate(
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
