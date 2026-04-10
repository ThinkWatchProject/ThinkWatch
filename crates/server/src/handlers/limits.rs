// ============================================================================
// Limits CRUD endpoints
//
// Thin REST surface over `think_watch_common::limits` for the admin
// console. Three resources, all keyed off a `(subject_kind, subject_id)`
// path tuple:
//
//   GET    /api/admin/limits/{kind}/{id}/rules    list rate-limit rules
//   POST   /api/admin/limits/{kind}/{id}/rules    upsert one rule
//   DELETE /api/admin/limits/{kind}/{id}/rules/{rule_id}
//
//   GET    /api/admin/limits/{kind}/{id}/budgets  list budget caps
//   POST   /api/admin/limits/{kind}/{id}/budgets  upsert one cap
//   DELETE /api/admin/limits/{kind}/{id}/budgets/{cap_id}
//
//   GET    /api/admin/limits/{kind}/{id}/usage    current count + spend
//
// Auth model:
//   - All endpoints require the matching `rate_limits:*` perm
//     (`:read` for GETs, `:write` for POST/DELETE).
//   - **Every endpoint also runs `assert_scope_for_subject`** —
//     the caller must hold the perm in a scope (global, or a team
//     containing the target subject) that covers `(kind, id)` from
//     the URL path. So a team_manager scoped to team:engineering
//     can edit limits on api_keys belonging to engineering members
//     but gets 403 trying to touch marketing's keys.
//   - Provider / mcp_server subjects always require global scope
//     because they're platform-wide resources — see
//     `AuthUser::assert_scope_for_subject`'s polymorphic dispatch.
// ============================================================================

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::limits::{
    self, BudgetCap, BudgetPeriod, BudgetSubject, RateLimitRule, RateLimitSubject, RateMetric,
    Surface, UpsertRule, budget, sliding,
};

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ----------------------------------------------------------------------------
// Path parameter parsing
// ----------------------------------------------------------------------------

/// Parse a `subject_kind` path segment for rate-limit rules. Rejects
/// any value not in the closed enum so callers get a 400 instead of
/// the engine silently returning an empty list.
fn parse_rate_subject(kind: &str) -> Result<RateLimitSubject, AppError> {
    RateLimitSubject::parse(kind).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown rate-limit subject kind '{kind}' (allowed: user, api_key, provider, mcp_server)"
        ))
    })
}

fn parse_budget_subject(kind: &str) -> Result<BudgetSubject, AppError> {
    BudgetSubject::parse(kind).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown budget subject kind '{kind}' (allowed: user, api_key, team, provider)"
        ))
    })
}

// ----------------------------------------------------------------------------
// Wire types — flat shapes the frontend can render directly without
// re-parsing the engine's enum types.
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RuleRow {
    pub id: Uuid,
    pub subject_kind: &'static str,
    pub subject_id: Uuid,
    pub surface: &'static str,
    pub metric: &'static str,
    pub window_secs: i32,
    pub max_count: i64,
    pub enabled: bool,
}

impl From<RateLimitRule> for RuleRow {
    fn from(r: RateLimitRule) -> Self {
        Self {
            id: r.id,
            subject_kind: r.subject_kind.as_str(),
            subject_id: r.subject_id,
            surface: r.surface.as_str(),
            metric: r.metric.as_str(),
            window_secs: r.window_secs,
            max_count: r.max_count,
            enabled: r.enabled,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CapRow {
    pub id: Uuid,
    pub subject_kind: &'static str,
    pub subject_id: Uuid,
    pub period: &'static str,
    pub limit_tokens: i64,
    pub enabled: bool,
}

impl From<BudgetCap> for CapRow {
    fn from(c: BudgetCap) -> Self {
        Self {
            id: c.id,
            subject_kind: c.subject_kind.as_str(),
            subject_id: c.subject_id,
            period: c.period.as_str(),
            limit_tokens: c.limit_tokens,
            enabled: c.enabled,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RuleListResponse {
    pub items: Vec<RuleRow>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CapListResponse {
    pub items: Vec<CapRow>,
}

// ----------------------------------------------------------------------------
// Request bodies — separated from the engine's `UpsertRule` because
// the wire shape uses string enum values, while the engine uses
// typed enums. Conversion happens in the handler.
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpsertRuleRequest {
    pub surface: String,
    pub metric: String,
    pub window_secs: i32,
    pub max_count: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpsertCapRequest {
    pub period: String,
    pub limit_tokens: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

// ----------------------------------------------------------------------------
// Rate-limit rules CRUD
// ----------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/limits/{kind}/{id}/rules",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
    ),
    responses(
        (status = 200, description = "List of rate-limit rules", body = RuleListResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn list_rules(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id)): Path<(String, Uuid)>,
) -> Result<Json<RuleListResponse>, AppError> {
    auth_user.require_permission("rate_limits:read")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:read", &kind, subject_id)
        .await?;
    let subject_kind = parse_rate_subject(&kind)?;
    let rules = limits::list_rules(&state.db, subject_kind, subject_id).await?;
    Ok(Json(RuleListResponse {
        items: rules.into_iter().map(RuleRow::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/{kind}/{id}/rules",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
    ),
    request_body = UpsertRuleRequest,
    responses(
        (status = 200, description = "Rule upserted", body = RuleRow),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn upsert_rule(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id)): Path<(String, Uuid)>,
    Json(req): Json<UpsertRuleRequest>,
) -> Result<Json<RuleRow>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", &kind, subject_id)
        .await?;
    let subject_kind = parse_rate_subject(&kind)?;
    let surface = Surface::parse(&req.surface).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown surface '{}' (allowed: ai_gateway, mcp_gateway)",
            req.surface
        ))
    })?;
    let metric = RateMetric::parse(&req.metric).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown metric '{}' (allowed: requests, tokens)",
            req.metric
        ))
    })?;

    // The engine validates window_secs / max_count and bubbles
    // sqlx::Error::Protocol on bad input — translate that into a
    // 400 instead of a 500 so the UI surfaces the message.
    let row = limits::upsert_rule(
        &state.db,
        UpsertRule {
            subject_kind,
            subject_id,
            surface,
            metric,
            window_secs: req.window_secs,
            max_count: req.max_count,
            enabled: req.enabled,
        },
    )
    .await
    .map_err(map_validation_error)?;

    // Notify other gateway pods to drop their cached rule sets so
    // the change takes effect immediately. Best-effort; a missed
    // notification just means the change waits for the next refresh.
    limits::notify_limits_changed(&state.redis).await;

    state.audit.log(
        auth_user
            .audit("rate_limit.upsert")
            .resource("rate_limit_rule")
            .resource_id(row.id.to_string())
            .detail(serde_json::json!({
                "subject_kind": kind,
                "subject_id": subject_id,
                "surface": req.surface,
                "metric": req.metric,
                "window_secs": req.window_secs,
                "max_count": req.max_count,
                "enabled": req.enabled,
            })),
    );

    Ok(Json(RuleRow::from(row)))
}

#[utoipa::path(
    delete,
    path = "/api/admin/limits/{kind}/{id}/rules/{rule_id}",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
        ("rule_id" = uuid::Uuid, Path, description = "Rate-limit rule ID"),
    ),
    responses(
        (status = 200, description = "Rule deleted"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn delete_rule(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id, rule_id)): Path<(String, Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", &kind, subject_id)
        .await?;
    // Validate the kind even though we don't actually need it for
    // the delete — keeps the URL shape consistent with the rest of
    // the surface.
    parse_rate_subject(&kind)?;
    let removed = limits::delete_rule(&state.db, rule_id).await?;
    if !removed {
        return Err(AppError::NotFound("Rate limit rule not found".into()));
    }
    limits::notify_limits_changed(&state.redis).await;
    state.audit.log(
        auth_user
            .audit("rate_limit.delete")
            .resource("rate_limit_rule")
            .resource_id(rule_id.to_string()),
    );
    Ok(Json(serde_json::json!({"deleted": true})))
}

// ----------------------------------------------------------------------------
// Budget caps CRUD
// ----------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/limits/{kind}/{id}/budgets",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, team, provider)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
    ),
    responses(
        (status = 200, description = "List of budget caps", body = CapListResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn list_caps(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id)): Path<(String, Uuid)>,
) -> Result<Json<CapListResponse>, AppError> {
    auth_user.require_permission("rate_limits:read")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:read", &kind, subject_id)
        .await?;
    let subject_kind = parse_budget_subject(&kind)?;
    let caps = limits::list_caps(&state.db, subject_kind, subject_id).await?;
    Ok(Json(CapListResponse {
        items: caps.into_iter().map(CapRow::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/{kind}/{id}/budgets",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, team, provider)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
    ),
    request_body = UpsertCapRequest,
    responses(
        (status = 200, description = "Budget cap upserted", body = CapRow),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn upsert_cap(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id)): Path<(String, Uuid)>,
    Json(req): Json<UpsertCapRequest>,
) -> Result<Json<CapRow>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", &kind, subject_id)
        .await?;
    let subject_kind = parse_budget_subject(&kind)?;
    let period = BudgetPeriod::parse(&req.period).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown period '{}' (allowed: daily, weekly, monthly)",
            req.period
        ))
    })?;

    let row = limits::upsert_cap(
        &state.db,
        subject_kind,
        subject_id,
        period,
        req.limit_tokens,
        req.enabled,
    )
    .await
    .map_err(map_validation_error)?;

    limits::notify_limits_changed(&state.redis).await;

    state.audit.log(
        auth_user
            .audit("budget_cap.upsert")
            .resource("budget_cap")
            .resource_id(row.id.to_string())
            .detail(serde_json::json!({
                "subject_kind": kind,
                "subject_id": subject_id,
                "period": req.period,
                "limit_tokens": req.limit_tokens,
                "enabled": req.enabled,
            })),
    );

    Ok(Json(CapRow::from(row)))
}

#[utoipa::path(
    delete,
    path = "/api/admin/limits/{kind}/{id}/budgets/{cap_id}",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, team, provider)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
        ("cap_id" = uuid::Uuid, Path, description = "Budget cap ID"),
    ),
    responses(
        (status = 200, description = "Budget cap deleted"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn delete_cap(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id, cap_id)): Path<(String, Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:write", &kind, subject_id)
        .await?;
    parse_budget_subject(&kind)?;
    let removed = limits::delete_cap(&state.db, cap_id).await?;
    if !removed {
        return Err(AppError::NotFound("Budget cap not found".into()));
    }
    limits::notify_limits_changed(&state.redis).await;
    state.audit.log(
        auth_user
            .audit("budget_cap.delete")
            .resource("budget_cap")
            .resource_id(cap_id.to_string()),
    );
    Ok(Json(serde_json::json!({"deleted": true})))
}

// ----------------------------------------------------------------------------
// Usage endpoint
//
// One read-only endpoint that returns the current count for every
// rate-limit rule + the current spend for every budget cap on a
// given subject. The frontend renders this as "used / limit" badges
// inline with the editor.
//
// Subject kind is the rate-limit kind here because that's the
// superset; budgets get queried with whichever subset overlaps.
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RuleUsage {
    pub rule_id: Uuid,
    pub current: i64,
    pub limit: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CapUsage {
    pub cap_id: Uuid,
    pub current: i64,
    pub limit: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UsageResponse {
    pub rules: Vec<RuleUsage>,
    pub caps: Vec<CapUsage>,
}

#[utoipa::path(
    get,
    path = "/api/admin/limits/{kind}/{id}/usage",
    tag = "Limits",
    security(("bearer_token" = [])),
    params(
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server)"),
        ("id" = uuid::Uuid, Path, description = "Subject ID"),
    ),
    responses(
        (status = 200, description = "Current usage counters for all rules and caps", body = UsageResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn get_usage(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((kind, subject_id)): Path<(String, Uuid)>,
) -> Result<Json<UsageResponse>, AppError> {
    auth_user.require_permission("rate_limits:read")?;
    auth_user
        .assert_scope_for_subject(&state.db, "rate_limits:read", &kind, subject_id)
        .await?;

    // Rate-limit rules: every rule for this subject (even disabled
    // ones), so the UI can show "this rule is paused but the
    // counter is still ticking" if a request mid-flight bumped it.
    let rate_subject = parse_rate_subject(&kind)?;
    let rules = limits::list_rules(&state.db, rate_subject, subject_id).await?;
    let mut rule_usage: Vec<RuleUsage> = Vec::with_capacity(rules.len());
    for r in &rules {
        let resolved = sliding::ResolvedRule {
            id: r.id,
            base_key: sliding::build_base_key(
                r.surface.as_str(),
                r.subject_kind.as_str(),
                r.subject_id,
                r.metric,
                r.window_secs,
            ),
            bucket_secs: sliding::bucket_secs(r.window_secs),
            max_count: r.max_count,
        };
        let current = sliding::current_count(&state.redis, &resolved).await;
        rule_usage.push(RuleUsage {
            rule_id: r.id,
            current,
            limit: r.max_count,
        });
    }

    // Budget caps: same subject_id, but only the kinds budgets
    // support. user / api_key / provider overlap; mcp_server has
    // no budget surface so it returns an empty list.
    let cap_usage: Vec<CapUsage> = if let Some(budget_kind) = budget_kind_for(rate_subject) {
        let caps = limits::list_caps(&state.db, budget_kind, subject_id).await?;
        let statuses = budget::current_spend(&state.redis, &caps)
            .await
            .unwrap_or_default();
        statuses
            .into_iter()
            .map(|s| CapUsage {
                cap_id: s.cap_id,
                current: s.current,
                limit: s.limit,
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(Json(UsageResponse {
        rules: rule_usage,
        caps: cap_usage,
    }))
}

/// Translate a rate-limit subject into the matching budget subject
/// when one exists. mcp_server doesn't have a budget side, so
/// callers fall back to "no caps".
fn budget_kind_for(subject: RateLimitSubject) -> Option<BudgetSubject> {
    match subject {
        RateLimitSubject::User => Some(BudgetSubject::User),
        RateLimitSubject::ApiKey => Some(BudgetSubject::ApiKey),
        RateLimitSubject::Provider => Some(BudgetSubject::Provider),
        RateLimitSubject::McpServer => None,
    }
}

// ----------------------------------------------------------------------------
// Error mapping
// ----------------------------------------------------------------------------

/// The engine returns `sqlx::Error::Protocol` for input validation
/// failures (window_secs out of range, max_count <= 0, etc). Those
/// should reach the client as 400, not 500. Anything else stays as
/// an internal sqlx error.
fn map_validation_error(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Protocol(msg) => AppError::BadRequest(msg.clone()),
        _ => AppError::from(e),
    }
}
