// ============================================================================
// User limits dashboard
//
// Consolidates three views an admin needs when investigating a single
// user's runtime quota state:
//
//   GET  /api/admin/users/{user_id}/limits-dashboard
//     → the effective policy (role-merged + overrides applied), with
//       live usage counters, paired with the 7-day token/request
//       series from gateway_logs and the last N limits-related audit
//       events.
//
//   POST /api/admin/users/{user_id}/limits/reset
//     → nukes the Redis counter(s) backing one rule or cap so the
//       user gets a fresh window without waiting for the existing one
//       to roll over.
//
// Both endpoints require `rate_limits:write` — reading the dashboard
// is still "write-class" admin info (includes reasons + who applied
// overrides); the usage counters themselves are not directly
// sensitive, but consolidating them behind the same gate keeps the
// policy simple.
// ============================================================================

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use fred::interfaces::KeysInterface;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::rbac;
use think_watch_common::errors::AppError;
use think_watch_common::limits::{
    self, BudgetCap, BudgetPeriod, BudgetSubject, RateLimitRule, RateLimitSubject, RateMetric,
    Surface, budget, sliding,
};

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ----------------------------------------------------------------------------
// Wire types
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EffectiveRule {
    /// `"role"` when the entry came from the merged role policy,
    /// `"override"` when a user side-table row replaced it. Frontend
    /// colors rows accordingly so operators can see at a glance
    /// which limits are custom.
    pub source: &'static str,
    /// UUID of the override row (only when `source == "override"`) —
    /// used as the action target for disable / delete / reset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_id: Option<Uuid>,
    pub surface: &'static str,
    pub metric: &'static str,
    pub window_secs: i32,
    pub max_count: i64,
    /// Role-derived value before the override replaced it. Present
    /// only when the override diverges — lets the UI show `↑5x` style
    /// comparison chips without an extra request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_default_max_count: Option<i64>,
    /// Current running count in the sliding window (sum across all
    /// buckets). Zero if no traffic yet.
    pub current: i64,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EffectiveCap {
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_id: Option<Uuid>,
    pub surface: &'static str,
    pub period: &'static str,
    pub limit_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_default_limit_tokens: Option<i64>,
    pub current: i64,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UsageDay {
    /// ISO date string, one per day in the 7-day window (oldest first).
    pub day: String,
    pub tokens: i64,
    pub requests: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LimitsAuditEvent {
    pub id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_user_email: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LimitsDashboard {
    pub rules: Vec<EffectiveRule>,
    pub caps: Vec<EffectiveCap>,
    pub usage_7d: Vec<UsageDay>,
    pub recent_events: Vec<LimitsAuditEvent>,
}

// ----------------------------------------------------------------------------
// GET /api/admin/users/{user_id}/limits-dashboard
// ----------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/users/{user_id}/limits-dashboard",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(("user_id" = uuid::Uuid, Path, description = "User id")),
    responses((status = 200, description = "Effective policy + usage + audit", body = LimitsDashboard))
)]
pub async fn get_limits_dashboard(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<LimitsDashboard>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", "user", user_id)
        .await?;

    // 1. Role-only baseline (pre-override) — lets us diff against the
    //    effective view to mark which slots are overridden and surface
    //    the original role value in the UI.
    let role_only = rbac::compute_user_role_constraints(&state.db, user_id).await?;
    // 2. Effective view (role + overrides applied). Same function the
    //    gateway hot path uses, so the dashboard is guaranteed to
    //    match runtime enforcement.
    let effective = rbac::compute_user_surface_constraints(&state.db, user_id).await?;
    // 3. Raw override rows so we can attach `override_id`, expiry, and
    //    reason back to the effective entries.
    let rule_overrides =
        limits::list_enabled_rules_for_subjects(&state.db, &[(RateLimitSubject::User, user_id)])
            .await?;
    let cap_overrides =
        limits::list_enabled_caps_for_subjects(&state.db, &[(BudgetSubject::User, user_id)])
            .await?;

    // Materialize the effective rules + caps with usage and source tags.
    let rules =
        build_effective_rules(&state, user_id, &role_only, &effective, &rule_overrides).await;
    let caps = build_effective_caps(&state, user_id, &role_only, &effective, &cap_overrides).await;

    // 7-day usage series + audit tail. Both depend on ClickHouse; if
    // it's not configured (dev-lite setup) we silently return empty.
    let usage_7d = load_usage_series(&state, user_id).await;
    let recent_events = load_recent_events(&state, user_id).await;

    Ok(Json(LimitsDashboard {
        rules,
        caps,
        usage_7d,
        recent_events,
    }))
}

/// Build the row list for rate-limit rules. For each (surface, metric,
/// window) that exists in the EFFECTIVE view, decide whether it's
/// role-derived or override, attach the matching override row's id +
/// metadata when applicable, and query Redis for the live count.
async fn build_effective_rules(
    state: &AppState,
    user_id: Uuid,
    role_only: &limits::SurfaceConstraints,
    effective: &limits::SurfaceConstraints,
    override_rows: &[RateLimitRule],
) -> Vec<EffectiveRule> {
    let mut out = Vec::new();
    for surface in [Surface::AiGateway, Surface::McpGateway] {
        let Some(block) = effective.block(surface) else {
            continue;
        };
        for rule in &block.rules {
            let ov = override_rows.iter().find(|o| {
                o.surface == surface && o.metric == rule.metric && o.window_secs == rule.window_secs
            });
            let role_default_max_count = role_only
                .block(surface)
                .and_then(|b| {
                    b.rules
                        .iter()
                        .find(|r| r.metric == rule.metric && r.window_secs == rule.window_secs)
                })
                .map(|r| r.max_count);
            let source = if ov.is_some() { "override" } else { "role" };

            // Live count from Redis. Best-effort — a Redis outage
            // shouldn't 500 the whole dashboard, just show 0.
            let resolved = sliding::ResolvedRule {
                id: ov.map(|o| o.id).unwrap_or(Uuid::nil()),
                base_key: sliding::build_base_key(
                    surface.as_str(),
                    "user",
                    user_id,
                    rule.metric,
                    rule.window_secs,
                ),
                bucket_secs: sliding::bucket_secs(rule.window_secs),
                max_count: rule.max_count,
            };
            let current = sliding::current_count(&state.redis, &resolved).await;

            out.push(EffectiveRule {
                source,
                override_id: ov.map(|o| o.id),
                surface: surface.as_str(),
                metric: rule.metric.as_str(),
                window_secs: rule.window_secs,
                max_count: rule.max_count,
                // Only surface the comparison when it actually differs;
                // otherwise UI would render "↑1x" on every role row.
                role_default_max_count: match (source, role_default_max_count) {
                    ("override", Some(d)) if d != rule.max_count => Some(d),
                    _ => None,
                },
                current,
                enabled: rule.enabled,
                expires_at: ov.and_then(|o| o.expires_at),
                reason: ov.and_then(|o| o.reason.clone()),
                created_by: ov.and_then(|o| o.created_by),
            });
        }
    }
    out
}

/// Budget caps are not per-surface in the DB (see the `budget_caps`
/// UNIQUE constraint). Side-table as-constraints duplicates a user
/// override onto BOTH surfaces, so the effective view has one entry
/// per (surface, period). For display we collapse back to (period)
/// since the caps share a counter — no point showing "ai monthly
/// 2M" and "mcp monthly 2M" as two separate rows.
async fn build_effective_caps(
    state: &AppState,
    user_id: Uuid,
    role_only: &limits::SurfaceConstraints,
    effective: &limits::SurfaceConstraints,
    override_rows: &[BudgetCap],
) -> Vec<EffectiveCap> {
    // Gather unique period cells. If both surfaces have the same
    // period, the values are identical by construction (side-table
    // budgets duplicate onto both surfaces) — pick the first.
    // Period is a small closed enum so a Vec scan is fine.
    let mut by_period: Vec<(BudgetPeriod, &limits::SurfaceBudget, Surface)> = Vec::new();
    for surface in [Surface::AiGateway, Surface::McpGateway] {
        let Some(block) = effective.block(surface) else {
            continue;
        };
        for b in &block.budgets {
            if !by_period.iter().any(|(p, _, _)| *p == b.period) {
                by_period.push((b.period, b, surface));
            }
        }
    }

    let mut out = Vec::new();
    for (period, budget_entry, surface) in by_period {
        let ov = override_rows.iter().find(|o| o.period == period);
        let role_default_limit = role_only
            .block(surface)
            .and_then(|b| b.budgets.iter().find(|x| x.period == period))
            .map(|b| b.limit_tokens);
        let source = if ov.is_some() { "override" } else { "role" };

        // Build a stand-in BudgetCap to read the current spend key.
        let cap = BudgetCap {
            id: ov.map(|o| o.id).unwrap_or(Uuid::nil()),
            subject_kind: BudgetSubject::User,
            subject_id: user_id,
            period,
            limit_tokens: budget_entry.limit_tokens,
            enabled: budget_entry.enabled,
            expires_at: None,
            reason: None,
            created_by: None,
        };
        let current = budget::current_spend(&state.redis, std::slice::from_ref(&cap))
            .await
            .ok()
            .and_then(|v| v.into_iter().next().map(|s| s.current))
            .unwrap_or(0);

        out.push(EffectiveCap {
            source,
            override_id: ov.map(|o| o.id),
            surface: surface.as_str(),
            period: period.as_str(),
            limit_tokens: budget_entry.limit_tokens,
            role_default_limit_tokens: match (source, role_default_limit) {
                ("override", Some(d)) if d != budget_entry.limit_tokens => Some(d),
                _ => None,
            },
            current,
            enabled: budget_entry.enabled,
            expires_at: ov.and_then(|o| o.expires_at),
            reason: ov.and_then(|o| o.reason.clone()),
            created_by: ov.and_then(|o| o.created_by),
        });
    }
    out
}

async fn load_usage_series(state: &AppState, user_id: Uuid) -> Vec<UsageDay> {
    #[derive(clickhouse::Row, Deserialize)]
    struct Row {
        day: String,
        tokens: i64,
        requests: i64,
    }
    let Some(ref ch) = state.clickhouse else {
        return Vec::new();
    };
    let sql = "\
        SELECT toString(toDate(created_at)) AS day, \
               toInt64(sum(ifNull(input_tokens, 0) + ifNull(output_tokens, 0))) AS tokens, \
               toInt64(count()) AS requests \
        FROM gateway_logs \
        WHERE user_id = ? AND created_at >= now() - INTERVAL 7 DAY \
        GROUP BY day \
        ORDER BY day ASC";
    match ch
        .query(sql)
        .bind(user_id.to_string())
        .fetch_all::<Row>()
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| UsageDay {
                day: r.day,
                tokens: r.tokens,
                requests: r.requests,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("limits-dashboard usage_7d query failed: {e}");
            Vec::new()
        }
    }
}

async fn load_recent_events(state: &AppState, user_id: Uuid) -> Vec<LimitsAuditEvent> {
    #[derive(clickhouse::Row, Deserialize)]
    struct Row {
        id: String,
        action: String,
        resource: Option<String>,
        resource_id: Option<String>,
        detail: Option<String>,
        user_email: Option<String>,
        #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
        created_at: DateTime<Utc>,
    }
    let Some(ref ch) = state.clickhouse else {
        return Vec::new();
    };
    // Include both sides of the audit trail:
    //   1. Rows where this user is the SUBJECT of the change (detail
    //      JSON carries subject_id=<user_id>). Covers upserts, bulk
    //      upserts, deletes, disables targeting this user.
    //   2. Rows where this user is the ACTOR (just the limits actions
    //      they took themselves) — rare for non-admins but useful
    //      when the dashboard is viewed for an admin account.
    let sql = "\
        SELECT id, action, resource, resource_id, detail, user_email, created_at \
        FROM audit_logs \
        WHERE resource IN ('rate_limit_rule', 'budget_cap') \
          AND (user_id = ? OR JSONExtractString(ifNull(detail, '{}'), 'subject_id') = ?) \
        ORDER BY created_at DESC \
        LIMIT 20";
    let user_id_str = user_id.to_string();
    match ch
        .query(sql)
        .bind(&user_id_str)
        .bind(&user_id_str)
        .fetch_all::<Row>()
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| LimitsAuditEvent {
                id: r.id,
                action: r.action,
                resource: r.resource,
                resource_id: r.resource_id,
                detail: r
                    .detail
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok()),
                actor_user_email: r.user_email,
                created_at: r.created_at.to_rfc3339(),
            })
            .collect(),
        Err(e) => {
            tracing::warn!("limits-dashboard recent_events query failed: {e}");
            Vec::new()
        }
    }
}

// ----------------------------------------------------------------------------
// POST /api/admin/users/{user_id}/limits/reset — nuke Redis counters
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ResetCounterRequest {
    /// `"rule"` resets a sliding-window rate-limit counter (all 60
    /// buckets), `"cap"` resets a budget counter (current period only).
    pub kind: String,
    /// Target identifies the rule or cap by the effective view's
    /// (surface, metric, window_secs) or (period) tuple. We key on
    /// the enforcement shape rather than a row id so we can reset
    /// both role-derived AND override rows with the same endpoint.
    pub surface: Option<String>,
    pub metric: Option<String>,
    pub window_secs: Option<i32>,
    pub period: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ResetCounterResponse {
    pub deleted_keys: usize,
}

#[utoipa::path(
    post,
    path = "/api/admin/users/{user_id}/limits/reset",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(("user_id" = uuid::Uuid, Path, description = "User id")),
    request_body = ResetCounterRequest,
    responses((status = 200, description = "Counters reset", body = ResetCounterResponse))
)]
pub async fn reset_user_counter(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<ResetCounterRequest>,
) -> Result<Json<ResetCounterResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", "user", user_id)
        .await?;

    let deleted = match req.kind.as_str() {
        "rule" => reset_rule_counter(&state, user_id, &req).await?,
        "cap" => reset_cap_counter(&state, user_id, &req).await?,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown kind '{other}' (expected rule or cap)"
            )));
        }
    };

    // Audit — deliberately NOT under `rate_limit_rule` resource because
    // this is a counter mutation, not a rule config change. A distinct
    // action name lets the dashboard surface "counter resets" as their
    // own event class if we later want to tally them.
    state.audit.log(
        auth_user
            .audit("rate_limit.counter_reset")
            .resource("rate_limit_counter")
            .detail(serde_json::json!({
                "subject_id": user_id,
                "kind": req.kind,
                "surface": req.surface,
                "metric": req.metric,
                "window_secs": req.window_secs,
                "period": req.period,
                "deleted_keys": deleted,
            })),
    );

    Ok(Json(ResetCounterResponse {
        deleted_keys: deleted,
    }))
}

async fn reset_rule_counter(
    state: &AppState,
    user_id: Uuid,
    req: &ResetCounterRequest,
) -> Result<usize, AppError> {
    let surface = Surface::parse(req.surface.as_deref().unwrap_or(""))
        .ok_or_else(|| AppError::BadRequest("surface is required for rule reset".into()))?;
    let metric = RateMetric::parse(req.metric.as_deref().unwrap_or(""))
        .ok_or_else(|| AppError::BadRequest("metric is required for rule reset".into()))?;
    let window_secs = req
        .window_secs
        .ok_or_else(|| AppError::BadRequest("window_secs is required for rule reset".into()))?;

    let base_key = sliding::build_base_key(surface.as_str(), "user", user_id, metric, window_secs);
    // Buckets are timestamp-derived (`now_secs / bucket_secs`). DEL the
    // 60-window range plus a small safety margin in case a late write
    // lands after we read the clock.
    let bucket_secs = sliding::bucket_secs(window_secs) as i64;
    if bucket_secs <= 0 {
        return Ok(0);
    }
    let now = chrono::Utc::now().timestamp();
    let current_bucket = now / bucket_secs;
    let mut deleted = 0usize;
    for b in 0..(sliding::BUCKETS_PER_WINDOW + 2) {
        let key = format!("{}:{}", base_key, current_bucket - b);
        match state.redis.del::<u64, _>(&key).await {
            Ok(n) => deleted += n as usize,
            Err(e) => {
                tracing::warn!("reset_rule_counter DEL failed for {key}: {e}");
            }
        }
    }
    Ok(deleted)
}

async fn reset_cap_counter(
    state: &AppState,
    user_id: Uuid,
    req: &ResetCounterRequest,
) -> Result<usize, AppError> {
    let period = BudgetPeriod::parse(req.period.as_deref().unwrap_or(""))
        .ok_or_else(|| AppError::BadRequest("period is required for cap reset".into()))?;
    let key = budget::build_key("user", user_id, period.as_str(), chrono::Utc::now());
    match state.redis.del::<u64, _>(&key).await {
        Ok(n) => Ok(n as usize),
        Err(e) => {
            tracing::warn!("reset_cap_counter DEL failed for {key}: {e}");
            Ok(0)
        }
    }
}
