// ============================================================================
// Bulk limit override operations
//
// Three endpoints layered on the existing side-table engine:
//
//   POST   /api/admin/limits/bulk/rules            apply one rule spec to N subjects
//   POST   /api/admin/limits/bulk/budgets          apply one cap spec to N subjects
//   POST   /api/admin/limits/bulk/rules/disable    flip enabled=false on N rule ids
//   POST   /api/admin/limits/bulk/rules/delete     hard-delete N rule ids
//   POST   /api/admin/limits/bulk/budgets/disable  flip enabled=false on N cap ids
//   POST   /api/admin/limits/bulk/budgets/delete   hard-delete N cap ids
//
// All-or-nothing is NOT the semantic — each row is attempted
// independently so one failing validation doesn't block the rest.
// Responses carry per-subject outcomes so the UI can show which users
// got the override and which didn't.
//
// The per-subject scope check is run individually so a team-scoped
// admin can apply across their team members but gets a 403 slot for
// any id outside their scope. Same auth gate as single-subject writes.
// ============================================================================

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::limits::{
    self, BudgetPeriod, BudgetSubject, RateLimitSubject, RateMetric, Surface, UpsertCap, UpsertRule,
};

use super::limits::validate_override_meta_pub;
use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ----------------------------------------------------------------------------
// Request shapes
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkApplyRuleRequest {
    /// Target subjects — one rule row is upserted per entry.
    pub targets: Vec<SubjectRef>,
    pub surface: String,
    pub metric: String,
    pub window_secs: i32,
    pub max_count: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkApplyCapRequest {
    pub targets: Vec<SubjectRef>,
    pub period: String,
    pub limit_tokens: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone, utoipa::ToSchema)]
pub struct SubjectRef {
    /// `"user"` or `"api_key"`. Consistent across all bulk endpoints.
    pub kind: String,
    pub id: Uuid,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkIdsRequest {
    pub ids: Vec<Uuid>,
}

fn default_true() -> bool {
    true
}

// ----------------------------------------------------------------------------
// Response shapes
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkOutcome {
    /// Echo of the input subject so the UI can line outcomes up with
    /// its selection table.
    pub subject_kind: String,
    pub subject_id: Uuid,
    /// `Some(id)` when the row was persisted; `None` when `error` is
    /// set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_id: Option<Uuid>,
    /// Short human-readable failure reason. Also mirrored in the
    /// audit log for the failure cases we wrote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkApplyResponse {
    pub outcomes: Vec<BulkOutcome>,
    pub success_count: usize,
    pub error_count: usize,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkIdsOutcome {
    pub id: Uuid,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BulkIdsResponse {
    pub outcomes: Vec<BulkIdsOutcome>,
    pub success_count: usize,
    pub error_count: usize,
}

// ----------------------------------------------------------------------------
// Handlers — apply same override to many subjects
// ----------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/rules",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkApplyRuleRequest,
    responses(
        (status = 200, description = "Per-subject outcomes", body = BulkApplyResponse),
        (status = 400, description = "Validation error on the shared override spec"),
        (status = 403, description = "Caller lacks rate_limits:write"),
    )
)]
pub async fn bulk_apply_rule(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkApplyRuleRequest>,
) -> Result<Json<BulkApplyResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    if req.targets.is_empty() {
        return Err(AppError::BadRequest("targets cannot be empty".into()));
    }
    if req.targets.len() > 500 {
        return Err(AppError::BadRequest(
            "targets capped at 500 per request".into(),
        ));
    }

    // Parse and validate the shared spec ONCE. A per-row failure here
    // would mean every row also fails — fast-fail with 400 so the UI
    // surfaces the issue before the operator thinks it half-applied.
    let surface = Surface::parse(&req.surface)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown surface '{}'", req.surface)))?;
    let metric = RateMetric::parse(&req.metric)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown metric '{}'", req.metric)))?;
    let (expires_at, reason) = validate_override_meta_pub(req.expires_at, req.reason)?;
    let actor_id = auth_user.claims.sub;

    let mut outcomes = Vec::with_capacity(req.targets.len());
    let mut success = 0usize;
    let mut errors = 0usize;

    for target in &req.targets {
        let kind = match RateLimitSubject::parse(&target.kind) {
            Some(k) => k,
            None => {
                errors += 1;
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: None,
                    error: Some(format!("Unknown subject kind '{}'", target.kind)),
                });
                continue;
            }
        };

        if let Err(e) = auth_user
            .assert_scope_for_subject(&state.db, "rate_limits:write", &target.kind, target.id)
            .await
        {
            errors += 1;
            outcomes.push(BulkOutcome {
                subject_kind: target.kind.clone(),
                subject_id: target.id,
                row_id: None,
                error: Some(format!("{e}")),
            });
            continue;
        }

        let result = limits::upsert_rule(
            &state.db,
            UpsertRule {
                subject_kind: kind,
                subject_id: target.id,
                surface,
                metric,
                window_secs: req.window_secs,
                max_count: req.max_count,
                enabled: req.enabled,
                expires_at,
                reason: reason.clone(),
                created_by: Some(actor_id),
            },
        )
        .await;

        match result {
            Ok(row) => {
                success += 1;
                state.audit.log(
                    auth_user
                        .audit("rate_limit.bulk_upsert")
                        .resource("rate_limit_rule")
                        .resource_id(row.id.to_string())
                        .detail(serde_json::json!({
                            "subject_kind": target.kind,
                            "subject_id": target.id,
                            "surface": req.surface,
                            "metric": req.metric,
                            "window_secs": req.window_secs,
                            "max_count": req.max_count,
                            "expires_at": expires_at,
                            "reason": reason,
                        })),
                );
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: Some(row.id),
                    error: None,
                });
            }
            Err(e) => {
                errors += 1;
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: None,
                    error: Some(db_error_message(&e)),
                });
            }
        }
    }

    if success > 0 {
        limits::notify_limits_changed(&state.redis).await;
    }

    Ok(Json(BulkApplyResponse {
        outcomes,
        success_count: success,
        error_count: errors,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/budgets",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkApplyCapRequest,
    responses(
        (status = 200, description = "Per-subject outcomes", body = BulkApplyResponse),
    )
)]
pub async fn bulk_apply_cap(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkApplyCapRequest>,
) -> Result<Json<BulkApplyResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    if req.targets.is_empty() {
        return Err(AppError::BadRequest("targets cannot be empty".into()));
    }
    if req.targets.len() > 500 {
        return Err(AppError::BadRequest(
            "targets capped at 500 per request".into(),
        ));
    }

    let period = BudgetPeriod::parse(&req.period)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown period '{}'", req.period)))?;
    let (expires_at, reason) = validate_override_meta_pub(req.expires_at, req.reason)?;
    let actor_id = auth_user.claims.sub;

    let mut outcomes = Vec::with_capacity(req.targets.len());
    let mut success = 0usize;
    let mut errors = 0usize;

    for target in &req.targets {
        let kind = match BudgetSubject::parse(&target.kind) {
            Some(k) => k,
            None => {
                errors += 1;
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: None,
                    error: Some(format!("Unknown subject kind '{}'", target.kind)),
                });
                continue;
            }
        };

        if let Err(e) = auth_user
            .assert_scope_for_subject(&state.db, "rate_limits:write", &target.kind, target.id)
            .await
        {
            errors += 1;
            outcomes.push(BulkOutcome {
                subject_kind: target.kind.clone(),
                subject_id: target.id,
                row_id: None,
                error: Some(format!("{e}")),
            });
            continue;
        }

        let result = limits::upsert_cap(
            &state.db,
            UpsertCap {
                subject_kind: kind,
                subject_id: target.id,
                period,
                limit_tokens: req.limit_tokens,
                enabled: req.enabled,
                expires_at,
                reason: reason.clone(),
                created_by: Some(actor_id),
            },
        )
        .await;

        match result {
            Ok(row) => {
                success += 1;
                state.audit.log(
                    auth_user
                        .audit("budget_cap.bulk_upsert")
                        .resource("budget_cap")
                        .resource_id(row.id.to_string())
                        .detail(serde_json::json!({
                            "subject_kind": target.kind,
                            "subject_id": target.id,
                            "period": req.period,
                            "limit_tokens": req.limit_tokens,
                            "expires_at": expires_at,
                            "reason": reason,
                        })),
                );
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: Some(row.id),
                    error: None,
                });
            }
            Err(e) => {
                errors += 1;
                outcomes.push(BulkOutcome {
                    subject_kind: target.kind.clone(),
                    subject_id: target.id,
                    row_id: None,
                    error: Some(db_error_message(&e)),
                });
            }
        }
    }

    if success > 0 {
        limits::notify_limits_changed(&state.redis).await;
    }

    Ok(Json(BulkApplyResponse {
        outcomes,
        success_count: success,
        error_count: errors,
    }))
}

// ----------------------------------------------------------------------------
// Handlers — bulk disable / delete by row id
// ----------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/rules/disable",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkIdsRequest,
    responses((status = 200, description = "Per-id outcomes", body = BulkIdsResponse))
)]
pub async fn bulk_disable_rules(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkIdsRequest>,
) -> Result<Json<BulkIdsResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    run_bulk_id_op(
        &state,
        &auth_user,
        &req.ids,
        "rate_limit_rules",
        BulkIdOp::Disable,
        |row_id| {
            auth_user
                .audit("rate_limit.bulk_disable")
                .resource("rate_limit_rule")
                .resource_id(row_id.to_string())
        },
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/rules/delete",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkIdsRequest,
    responses((status = 200, description = "Per-id outcomes", body = BulkIdsResponse))
)]
pub async fn bulk_delete_rules(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkIdsRequest>,
) -> Result<Json<BulkIdsResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    run_bulk_id_op(
        &state,
        &auth_user,
        &req.ids,
        "rate_limit_rules",
        BulkIdOp::Delete,
        |row_id| {
            auth_user
                .audit("rate_limit.bulk_delete")
                .resource("rate_limit_rule")
                .resource_id(row_id.to_string())
        },
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/budgets/disable",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkIdsRequest,
    responses((status = 200, description = "Per-id outcomes", body = BulkIdsResponse))
)]
pub async fn bulk_disable_caps(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkIdsRequest>,
) -> Result<Json<BulkIdsResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    run_bulk_id_op(
        &state,
        &auth_user,
        &req.ids,
        "budget_caps",
        BulkIdOp::Disable,
        |row_id| {
            auth_user
                .audit("budget_cap.bulk_disable")
                .resource("budget_cap")
                .resource_id(row_id.to_string())
        },
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/admin/limits/bulk/budgets/delete",
    tag = "Limits",
    security(("bearer_token" = [])),
    request_body = BulkIdsRequest,
    responses((status = 200, description = "Per-id outcomes", body = BulkIdsResponse))
)]
pub async fn bulk_delete_caps(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkIdsRequest>,
) -> Result<Json<BulkIdsResponse>, AppError> {
    auth_user.require_permission("rate_limits:write")?;
    run_bulk_id_op(
        &state,
        &auth_user,
        &req.ids,
        "budget_caps",
        BulkIdOp::Delete,
        |row_id| {
            auth_user
                .audit("budget_cap.bulk_delete")
                .resource("budget_cap")
                .resource_id(row_id.to_string())
        },
    )
    .await
}

// ----------------------------------------------------------------------------
// Shared bulk-by-id machinery
// ----------------------------------------------------------------------------

enum BulkIdOp {
    Disable,
    Delete,
}

async fn run_bulk_id_op(
    state: &AppState,
    auth_user: &AuthUser,
    ids: &[Uuid],
    table: &'static str,
    op: BulkIdOp,
    audit: impl Fn(Uuid) -> think_watch_common::audit::AuditEntry,
) -> Result<Json<BulkIdsResponse>, AppError> {
    if ids.is_empty() {
        return Err(AppError::BadRequest("ids cannot be empty".into()));
    }
    if ids.len() > 500 {
        return Err(AppError::BadRequest("ids capped at 500 per request".into()));
    }

    // Per-id execution so one failure doesn't poison the batch.
    let mut outcomes = Vec::with_capacity(ids.len());
    let mut success = 0usize;
    let mut errors = 0usize;
    let mutate_sql = match op {
        BulkIdOp::Disable => {
            format!("UPDATE {table} SET enabled = FALSE, updated_at = now() WHERE id = $1")
        }
        BulkIdOp::Delete => format!("DELETE FROM {table} WHERE id = $1"),
    };
    // SECURITY: pre-flight scope check per row. The single-row
    // `delete_rule` / `delete_cap` handlers take `(kind, subject_id)`
    // in their URL path and call `assert_scope_for_subject` against
    // those values; the bulk variant takes only row ids, so we have
    // to look up each row's subject ourselves before mutating it.
    // Without this, a team-scoped caller with `rate_limits:write` can
    // pass arbitrary ids and disable / delete rows for users outside
    // their scope.
    let lookup_sql = format!("SELECT subject_kind, subject_id FROM {table} WHERE id = $1");

    for id in ids {
        let subject: Option<(String, Uuid)> = match sqlx::query_as(&lookup_sql)
            .bind(id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(row) => row,
            Err(e) => {
                errors += 1;
                outcomes.push(BulkIdsOutcome {
                    id: *id,
                    success: false,
                    error: Some(db_error_message(&e)),
                });
                continue;
            }
        };
        let Some((subject_kind, subject_id)) = subject else {
            errors += 1;
            outcomes.push(BulkIdsOutcome {
                id: *id,
                success: false,
                error: Some("not found".into()),
            });
            continue;
        };
        if let Err(e) = auth_user
            .assert_scope_for_subject(&state.db, "rate_limits:write", &subject_kind, subject_id)
            .await
        {
            errors += 1;
            outcomes.push(BulkIdsOutcome {
                id: *id,
                success: false,
                error: Some(format!("forbidden: {e}")),
            });
            continue;
        }

        let result = sqlx::query(&mutate_sql).bind(id).execute(&state.db).await;
        match result {
            Ok(r) if r.rows_affected() > 0 => {
                success += 1;
                state.audit.log(audit(*id));
                outcomes.push(BulkIdsOutcome {
                    id: *id,
                    success: true,
                    error: None,
                });
            }
            Ok(_) => {
                errors += 1;
                outcomes.push(BulkIdsOutcome {
                    id: *id,
                    success: false,
                    error: Some("not found".into()),
                });
            }
            Err(e) => {
                errors += 1;
                outcomes.push(BulkIdsOutcome {
                    id: *id,
                    success: false,
                    error: Some(db_error_message(&e)),
                });
            }
        }
    }

    if success > 0 {
        limits::notify_limits_changed(&state.redis).await;
    }

    Ok(Json(BulkIdsResponse {
        outcomes,
        success_count: success,
        error_count: errors,
    }))
}

fn db_error_message(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Protocol(msg) => msg.clone(),
        other => other.to_string(),
    }
}
