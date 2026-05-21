//! Materialize merged surface constraints into rate-limit rule rows
//! and budget caps keyed by the authenticated user id.

use uuid::Uuid;

use super::GatewayRequestIdentity;
use think_watch_common::limits::{
    BudgetCap, BudgetSubject, RateLimitRule, RateLimitSubject, Surface,
};

/// Materialize the merged surface constraints into `RateLimitRule`
/// rows keyed by the authenticated user id. Redis counters now live
/// at `ratelimit:<surface>:user:<user_id>:...` — one set per user
/// regardless of how many roles they hold. Roles merged to empty
/// (no user_id, or no rules) produce an empty list.
pub(super) fn rules_for_ai_gateway(identity: &GatewayRequestIdentity) -> Vec<RateLimitRule> {
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

pub(super) fn budgets_for_ai_gateway(identity: &GatewayRequestIdentity) -> Vec<BudgetCap> {
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
