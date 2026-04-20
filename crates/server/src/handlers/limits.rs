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
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::limits::{
    self, BudgetCap, BudgetPeriod, BudgetSubject, RateLimitRule, RateLimitSubject, RateMetric,
    Surface, UpsertCap, UpsertRule, budget, sliding,
};

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Hard ceiling on how far into the future a temporary override can
/// extend. Anything longer is almost certainly a "permanent" change
/// wearing the wrong hat — the operator should edit the role instead.
const MAX_OVERRIDE_HORIZON_DAYS: i64 = 90;
/// Minimum chars the operator must supply as justification. Short
/// enough not to be annoying, long enough to discourage "test" and ".".
const MIN_REASON_LEN: usize = 10;
const MAX_REASON_LEN: usize = 500;

// ----------------------------------------------------------------------------
// Path parameter parsing
// ----------------------------------------------------------------------------

// Role-scoped limits now live inline on `rbac_roles.surface_constraints`.
// The side-table endpoints explicitly refuse `role` so a stale client
// can't silently half-update one side.
const ROLE_INLINE_MSG: &str = "Role limits are inline on the role; use the role endpoint";

fn reject_role_kind(kind: &str) -> Result<(), AppError> {
    if kind == "role" {
        return Err(AppError::BadRequest(ROLE_INLINE_MSG.into()));
    }
    Ok(())
}

/// Parse a `subject_kind` path segment for rate-limit rules. Rejects
/// any value not in the closed enum so callers get a 400 instead of
/// the engine silently returning an empty list.
fn parse_rate_subject(kind: &str) -> Result<RateLimitSubject, AppError> {
    reject_role_kind(kind)?;
    RateLimitSubject::parse(kind).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown rate-limit subject kind '{kind}' (allowed: user, api_key)"
        ))
    })
}

fn parse_budget_subject(kind: &str) -> Result<BudgetSubject, AppError> {
    reject_role_kind(kind)?;
    BudgetSubject::parse(kind).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown budget subject kind '{kind}' (allowed: user, api_key)"
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
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
            expires_at: r.expires_at,
            reason: r.reason,
            created_by: r.created_by,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
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
            expires_at: c.expires_at,
            reason: c.reason,
            created_by: c.created_by,
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
    /// Optional UTC expiry. Must be in the future and within
    /// `MAX_OVERRIDE_HORIZON_DAYS`. Pair with `reason` so the audit
    /// trail makes sense.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Justification text. Required when `expires_at` is set — a
    /// bounded override without a documented reason is an anti-pattern.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpsertCapRequest {
    pub period: String,
    pub limit_tokens: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reason: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Public re-export of [`validate_override_meta`] for sibling modules
/// (bulk endpoints) — keeps the single validation path canonical.
pub(super) fn validate_override_meta_pub(
    expires_at: Option<DateTime<Utc>>,
    reason: Option<String>,
) -> Result<(Option<DateTime<Utc>>, Option<String>), AppError> {
    validate_override_meta(expires_at, reason)
}

/// Shared validation for override metadata. Returns the normalized
/// `(expires_at, reason)` pair on success. Called from both the
/// single-subject handlers and the bulk apply endpoint.
fn validate_override_meta(
    expires_at: Option<DateTime<Utc>>,
    reason: Option<String>,
) -> Result<(Option<DateTime<Utc>>, Option<String>), AppError> {
    let now = Utc::now();
    if let Some(t) = expires_at {
        if t <= now {
            return Err(AppError::BadRequest(
                "expires_at must be in the future".into(),
            ));
        }
        if t - now > Duration::days(MAX_OVERRIDE_HORIZON_DAYS) {
            return Err(AppError::BadRequest(format!(
                "expires_at must be within {MAX_OVERRIDE_HORIZON_DAYS} days — edit the role for a permanent change"
            )));
        }
    }
    let trimmed = reason
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if expires_at.is_some() {
        match &trimmed {
            Some(r) if r.len() >= MIN_REASON_LEN && r.len() <= MAX_REASON_LEN => {}
            Some(r) if r.len() < MIN_REASON_LEN => {
                return Err(AppError::BadRequest(format!(
                    "reason must be at least {MIN_REASON_LEN} characters"
                )));
            }
            Some(_) => {
                return Err(AppError::BadRequest(format!(
                    "reason must be at most {MAX_REASON_LEN} characters"
                )));
            }
            None => {
                return Err(AppError::BadRequest(
                    "reason is required when expires_at is set".into(),
                ));
            }
        }
    }
    Ok((expires_at, trimmed))
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
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server, team)"),
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
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server, team)"),
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

    let (expires_at, reason) = validate_override_meta(req.expires_at, req.reason)?;

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
            expires_at,
            reason: reason.clone(),
            // Audit trail — who wrote this row. Parsing from the
            // `sub` string should always succeed for authenticated
            // sessions; fall back to None if it somehow doesn't.
            created_by: Some(auth_user.claims.sub),
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
                "expires_at": expires_at,
                "reason": reason,
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
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server, team)"),
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

    let (expires_at, reason) = validate_override_meta(req.expires_at, req.reason)?;

    let row = limits::upsert_cap(
        &state.db,
        UpsertCap {
            subject_kind,
            subject_id,
            period,
            limit_tokens: req.limit_tokens,
            enabled: req.enabled,
            expires_at,
            reason: reason.clone(),
            created_by: Some(auth_user.claims.sub),
        },
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
                "expires_at": expires_at,
                "reason": reason,
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
        ("kind" = String, Path, description = "Subject kind (user, api_key, provider, mcp_server, team)"),
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

/// Translate a rate-limit subject into the matching budget subject.
/// The kinds overlap 1:1 today (both sides accept user / api_key).
fn budget_kind_for(subject: RateLimitSubject) -> Option<BudgetSubject> {
    match subject {
        RateLimitSubject::User => Some(BudgetSubject::User),
        RateLimitSubject::ApiKey => Some(BudgetSubject::ApiKey),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rate_subject_rejects_role() {
        let err = parse_rate_subject("role").unwrap_err();
        match err {
            AppError::BadRequest(msg) => assert_eq!(msg, ROLE_INLINE_MSG),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_budget_subject_rejects_role() {
        let err = parse_budget_subject("role").unwrap_err();
        match err {
            AppError::BadRequest(msg) => assert_eq!(msg, ROLE_INLINE_MSG),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_subjects_accept_user_and_api_key() {
        assert!(parse_rate_subject("user").is_ok());
        assert!(parse_rate_subject("api_key").is_ok());
        assert!(parse_budget_subject("user").is_ok());
        assert!(parse_budget_subject("api_key").is_ok());
    }

    #[test]
    fn override_meta_rejects_past_expiry() {
        let past = Utc::now() - Duration::minutes(1);
        let err = validate_override_meta(Some(past), Some("reason string".into())).unwrap_err();
        match err {
            AppError::BadRequest(m) => assert!(m.contains("future"), "got: {m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn override_meta_rejects_long_horizon() {
        let far_future = Utc::now() + Duration::days(MAX_OVERRIDE_HORIZON_DAYS + 1);
        let err =
            validate_override_meta(Some(far_future), Some("valid reason here".into())).unwrap_err();
        match err {
            AppError::BadRequest(m) => assert!(m.contains("days"), "got: {m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn override_meta_requires_reason_when_expiring() {
        let future = Utc::now() + Duration::hours(1);
        let err = validate_override_meta(Some(future), None).unwrap_err();
        match err {
            AppError::BadRequest(m) => assert!(m.contains("reason"), "got: {m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn override_meta_rejects_short_reason() {
        let future = Utc::now() + Duration::hours(1);
        let err = validate_override_meta(Some(future), Some("nope".into())).unwrap_err();
        match err {
            AppError::BadRequest(m) => assert!(m.contains("reason"), "got: {m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn override_meta_accepts_bounded_expiry_with_reason() {
        let future = Utc::now() + Duration::days(7);
        let (out_exp, out_reason) =
            validate_override_meta(Some(future), Some("  Black friday boost  ".into())).unwrap();
        assert_eq!(out_exp, Some(future));
        assert_eq!(out_reason.as_deref(), Some("Black friday boost"));
    }

    #[test]
    fn override_meta_no_expiry_allowed() {
        // Permanent override with no reason is still valid at this
        // layer — the UI nudges operators, the validator doesn't
        // force it when expires_at is None.
        let (exp, reason) = validate_override_meta(None, None).unwrap();
        assert!(exp.is_none());
        assert!(reason.is_none());
    }
}
