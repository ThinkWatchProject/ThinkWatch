use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use think_watch_auth::api_key;
use think_watch_common::audit::AuditEntry;
use think_watch_common::dto::{
    CreateApiKeyRequest, CreateApiKeyResponse, PaginatedResponse, PaginationParams,
};
use think_watch_common::errors::AppError;
use think_watch_common::models::ApiKey;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Resolve whether the caller sees only their own keys or the whole
/// table. API keys are user-owned, so scope collapses to two cases:
/// the caller has `perm` at GLOBAL scope (sees everything) or they
/// only see their own rows. Team-scoped grants no longer widen the
/// visible set because api_keys don't belong to teams.
async fn caller_has_global(
    auth_user: &AuthUser,
    pool: &sqlx::PgPool,
    perm: &str,
) -> Result<bool, AppError> {
    Ok(auth_user
        .owned_team_scope_for_perm(pool, perm)
        .await?
        .is_none())
}

/// Reject the request when the caller neither owns the target key
/// nor has the supplied permission at global scope. Used by the
/// single-key endpoints (GET/PATCH/DELETE/rotate).
async fn assert_owner_or_global(
    auth_user: &AuthUser,
    pool: &sqlx::PgPool,
    perm: &str,
    key_id: Uuid,
) -> Result<(), AppError> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
        .bind(key_id)
        .fetch_optional(pool)
        .await?;
    let owner = owner.ok_or_else(|| AppError::NotFound("API key not found".into()))?;
    let has_perm = !auth_user.denied_permissions.iter().any(|p| p == perm)
        && auth_user.permissions.iter().any(|p| p == perm);
    if !has_perm {
        return Err(AppError::Forbidden(format!(
            "Missing required permission: {perm}"
        )));
    }
    if auth_user.claims.sub == owner {
        return Ok(());
    }
    if caller_has_global(auth_user, pool, perm).await? {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "{perm} not granted for this API key"
    )))
}

/// GET /api/keys
///
/// Result set is the union of:
///   - the caller's own keys (always, when they hold `api_keys:read`)
///   - everyone's keys, when the caller has `api_keys:read` at
///     global scope (super_admin / admin case)
#[utoipa::path(
    get,
    path = "/api/keys",
    tag = "API Keys",
    params(
        ("page" = Option<u32>, Query, description = "Page number (1-based, default 1)"),
        ("per_page" = Option<u32>, Query, description = "Items per page (max 100, default 20)"),
    ),
    responses(
        (status = 200, description = "Paginated list of API keys visible to the caller"),
        (status = 401, description = "Unauthorized"),
    ),
)]
pub async fn list_keys(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedResponse<ApiKey>>, AppError> {
    auth_user.require_permission("api_keys:read")?;
    let per_page = pagination.per_page();
    let offset = pagination.offset();
    let caller_id = auth_user.claims.sub;
    let global = caller_has_global(&auth_user, &state.db, "api_keys:read").await?;

    let (total, keys): (i64, Vec<ApiKey>) = if global {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE deleted_at IS NULL")
                .fetch_one(&state.db)
                .await?;
        let keys = sqlx::query_as::<_, ApiKey>(
            "SELECT * FROM api_keys WHERE deleted_at IS NULL \
             ORDER BY created_at DESC LIMIT $1 OFFSET $2",
        )
        .bind(per_page as i64)
        .bind(offset as i64)
        .fetch_all(&state.db)
        .await?;
        (total, keys)
    } else {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys WHERE deleted_at IS NULL AND user_id = $1",
        )
        .bind(caller_id)
        .fetch_one(&state.db)
        .await?;
        let keys = sqlx::query_as::<_, ApiKey>(
            "SELECT * FROM api_keys WHERE deleted_at IS NULL AND user_id = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(caller_id)
        .bind(per_page as i64)
        .bind(offset as i64)
        .fetch_all(&state.db)
        .await?;
        (total, keys)
    };

    Ok(Json(PaginatedResponse {
        data: keys,
        total,
        page: pagination.page.unwrap_or(1).max(1),
        per_page,
    }))
}

/// Allowed values for the `surfaces` column. Kept in lockstep with
/// the DB CHECK constraint and with `RateLimitSubject::Surface` on
/// the limits engine — adding a new gateway means updating both.
/// Surface kinds accepted by `api_keys.surfaces`. Kept in lockstep with
/// the `api_key_surface_kinds` lookup table in migrations/001_init.sql —
/// the DB trigger is the final gate, this is the fast-path check so
/// handlers don't need a round-trip to reject a typo. Adding a surface
/// means editing both this const and the lookup-table seed.
pub(crate) const ALLOWED_SURFACES: &[&str] = &["ai_gateway", "mcp_gateway", "console"];

/// Validate + dedupe a caller-supplied surfaces list. Rejects unknown
/// values and empty input. Returns the normalized list (sorted,
/// deduped) so the DB row is stable across re-saves.
fn normalize_surfaces(input: &[String]) -> Result<Vec<String>, AppError> {
    if input.is_empty() {
        return Err(AppError::BadRequest(
            "API key must be enabled for at least one gateway surface".into(),
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(input.len());
    for s in input {
        if !ALLOWED_SURFACES.contains(&s.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Unknown surface '{s}' (allowed: ai_gateway, mcp_gateway, console)"
            )));
        }
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out.sort();
    Ok(out)
}

#[utoipa::path(
    post,
    path = "/api/keys",
    tag = "API Keys",
    request_body(
        content_type = "application/json",
        description = "Key name, surfaces, optional allowed_models, expiry, and cost_center",
    ),
    responses(
        (status = 200, description = "API key created — plaintext key shown only once"),
        (status = 400, description = "Invalid surfaces"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Missing api_keys:create permission"),
    ),
)]
#[tracing::instrument(skip_all, fields(handler = "api_keys.create_key"))]
pub async fn create_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>, AppError> {
    // Self-serve creation: any role granting `api_keys:create` lets the
    // caller mint a key bound to themselves. Keys no longer have a team
    // concept, so no cross-tenant gate is needed — the key inherits
    // permissions from the owner's roles only.
    auth_user.require_permission("api_keys:create")?;

    let surfaces = normalize_surfaces(&req.surfaces)?;

    let generated = api_key::generate_api_key();

    let expires_at = req
        .expires_in_days
        .map(|days| chrono::Utc::now() + chrono::Duration::days(days as i64));

    let cost_center = validate_cost_center(req.cost_center.as_deref())?;

    let row = sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, surfaces, allowed_models, expires_at, cost_center)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(&req.name)
    .bind(auth_user.claims.sub)
    .bind(&surfaces)
    .bind(&req.allowed_models)
    .bind(expires_at)
    .bind(cost_center.as_deref())
    .fetch_one(&state.db)
    .await?;

    Ok(Json(CreateApiKeyResponse {
        id: row.id,
        key: generated.plaintext, // shown only once!
        name: row.name,
        key_prefix: row.key_prefix,
    }))
}

#[utoipa::path(
    get,
    path = "/api/keys/{id}",
    tag = "API Keys",
    params(
        ("id" = Uuid, Path, description = "API key UUID"),
    ),
    responses(
        (status = 200, description = "API key details"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden — caller is not the owner and lacks global scope"),
        (status = 404, description = "API key not found"),
    ),
)]
pub async fn get_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApiKey>, AppError> {
    assert_owner_or_global(&auth_user, &state.db, "api_keys:read", id).await?;
    let key =
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound("API key not found".into()))?;

    Ok(Json(key))
}

#[utoipa::path(
    delete,
    path = "/api/keys/{id}",
    tag = "API Keys",
    params(
        ("id" = Uuid, Path, description = "API key UUID"),
    ),
    responses(
        (status = 200, description = "API key revoked"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "API key not found"),
    ),
)]
#[tracing::instrument(skip_all, fields(handler = "api_keys.revoke_key"))]
pub async fn revoke_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    assert_owner_or_global(&auth_user, &state.db, "api_keys:delete", id).await?;

    // Also clear grace_period_ends_at: the auth middleware accepts a
    // key that is either is_active=true OR still inside its rotation
    // grace window, so an is_active=false alone leaves a rotated key
    // usable until grace expires.
    let result = sqlx::query(
        "UPDATE api_keys SET is_active = false, grace_period_ends_at = NULL, \
                disabled_reason = 'revoked' \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("API key not found".into()));
    }

    state.audit.log(
        AuditEntry::new("api_key.revoke")
            .user_id(auth_user.claims.sub)
            .resource(format!("api_key:{id}")),
    );

    Ok(Json(serde_json::json!({"status": "revoked"})))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ForceRevokeRequest {
    /// Reason for the emergency revocation (e.g. "suspected leak",
    /// "employee offboarding"). Recorded verbatim in the audit log.
    pub reason: String,
}

/// POST /api/admin/keys/{id}/force-revoke — admin-only, immediate kill.
///
/// Distinct from the owner-accessible DELETE /api/keys/{id}: this
/// endpoint requires `api_keys:delete` at GLOBAL scope (no owner
/// path), mandates a reason, and is intended for compromise-response
/// workflows where an admin needs to kill any user's key without
/// waiting for the normal rotation grace window to elapse.
#[utoipa::path(
    post,
    path = "/api/admin/keys/{id}/force-revoke",
    tag = "API Keys",
    params(
        ("id" = Uuid, Path, description = "API key UUID"),
    ),
    request_body = ForceRevokeRequest,
    responses(
        (status = 200, description = "API key force-revoked"),
        (status = 400, description = "Reason is required"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden — requires global api_keys:delete"),
        (status = 404, description = "API key not found"),
    ),
)]
#[tracing::instrument(skip_all, fields(handler = "api_keys.force_revoke_key"))]
pub async fn force_revoke_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ForceRevokeRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("api_keys:delete")?;
    if !caller_has_global(&auth_user, &state.db, "api_keys:delete").await? {
        return Err(AppError::Forbidden(
            "Force-revoke requires api_keys:delete at global scope".into(),
        ));
    }
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("reason is required".into()));
    }
    if reason.len() > 500 {
        return Err(AppError::BadRequest(
            "reason must be 500 characters or fewer".into(),
        ));
    }

    // Truncate to a short disabled_reason tag; full reason goes to audit.
    let disabled_reason = format!(
        "force_revoked:{}",
        reason.chars().take(64).collect::<String>()
    );
    let result = sqlx::query(
        "UPDATE api_keys SET is_active = false, grace_period_ends_at = NULL, \
                disabled_reason = $1 \
          WHERE id = $2 AND deleted_at IS NULL",
    )
    .bind(&disabled_reason)
    .bind(id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("API key not found".into()));
    }

    state.audit.log(
        AuditEntry::new("api_key.force_revoke")
            .user_id(auth_user.claims.sub)
            .resource(format!("api_key:{id}"))
            .detail(serde_json::json!({ "reason": reason })),
    );

    Ok(Json(serde_json::json!({
        "status": "force_revoked",
        "reason": reason,
    })))
}

// --- API key lifecycle management ---

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateKeyRequest {
    /// When `Some(None-inside)` field omitted → leave unchanged. A JSON
    /// `null` for allowed_models means "all models"; a non-empty array
    /// means restrict to that list.
    pub allowed_models: Option<Vec<String>>,
    /// When `Some`, replaces the entire surfaces list. Must still
    /// be non-empty. Omit the field to leave surfaces untouched.
    pub surfaces: Option<Vec<String>>,
    pub expires_in_days: Option<i32>,
    pub rotation_period_days: Option<i32>,
    pub inactivity_timeout_days: Option<i32>,
    /// Free-form cost-center / project tag. `Some("")` clears the tag;
    /// `None` leaves it untouched.
    pub cost_center: Option<String>,
}

/// PATCH /api/keys/{id} — update key settings.
#[utoipa::path(
    patch,
    path = "/api/keys/{id}",
    tag = "API Keys",
    params(
        ("id" = Uuid, Path, description = "API key UUID"),
    ),
    request_body = UpdateKeyRequest,
    responses(
        (status = 200, description = "Updated API key details"),
        (status = 400, description = "Invalid surfaces"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "API key not found"),
    ),
)]
pub async fn update_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateKeyRequest>,
) -> Result<Json<ApiKey>, AppError> {
    assert_owner_or_global(&auth_user, &state.db, "api_keys:update", id).await?;
    let key =
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound("API key not found".into()))?;

    let normalized_surfaces = req
        .surfaces
        .as_deref()
        .map(normalize_surfaces)
        .transpose()?;

    let expires_at = req
        .expires_in_days
        .map(|days| {
            if days > 0 {
                Some(chrono::Utc::now() + chrono::Duration::days(days as i64))
            } else {
                None // 0 = no expiry
            }
        })
        .unwrap_or(key.expires_at.map(Some).unwrap_or(None));

    // cost_center semantics:
    //  * None              → leave unchanged
    //  * Some("")          → clear (NULL)
    //  * Some("anything")  → set after validation
    let (cost_center_set, cost_center_value) = match req.cost_center.as_deref() {
        None => (false, None),
        Some("") => (true, None),
        Some(s) => (true, validate_cost_center(Some(s))?),
    };

    // Detect an actual extension of expires_at (later than the current
    // value, or removed entirely). When that happens the lifecycle
    // task's `last_expiry_warning_days` dedupe column has to reset
    // — otherwise a key warned at 7-day-remaining and then extended
    // by 90 days would silently miss the next 7-day warning because
    // `bucket < last_expiry_warning_days` would still be false.
    let expiry_extended = match (key.expires_at, expires_at) {
        (Some(prev), Some(new_val)) => new_val > prev,
        (Some(_), None) => true, // expiry removed entirely
        _ => false,
    };

    let updated = sqlx::query_as::<_, ApiKey>(
        r#"UPDATE api_keys SET
            allowed_models = COALESCE($1, allowed_models),
            surfaces = COALESCE($2, surfaces),
            expires_at = $3,
            rotation_period_days = COALESCE($4, rotation_period_days),
            inactivity_timeout_days = COALESCE($5, inactivity_timeout_days),
            cost_center = CASE WHEN $7 THEN $6 ELSE cost_center END,
            last_expiry_warning_days = CASE WHEN $9 THEN NULL
                                            ELSE last_expiry_warning_days END
           WHERE id = $8 RETURNING *"#,
    )
    .bind(&req.allowed_models)
    .bind(normalized_surfaces.as_ref())
    .bind(expires_at)
    .bind(req.rotation_period_days)
    .bind(req.inactivity_timeout_days)
    .bind(cost_center_value.as_deref())
    .bind(cost_center_set)
    .bind(id)
    .bind(expiry_extended)
    .fetch_one(&state.db)
    .await?;

    // Record what actually changed in the audit detail. Surfaces /
    // allowed_models / limits carry real security weight, so capturing
    // before/after values lets admins trace who loosened a key's scope.
    let mut changes = serde_json::Map::new();
    if req.allowed_models.is_some() && req.allowed_models != key.allowed_models {
        changes.insert(
            "allowed_models".into(),
            serde_json::json!({
                "before": key.allowed_models,
                "after": updated.allowed_models,
            }),
        );
    }
    if normalized_surfaces.is_some()
        && normalized_surfaces.as_deref() != Some(key.surfaces.as_slice())
    {
        changes.insert(
            "surfaces".into(),
            serde_json::json!({
                "before": key.surfaces,
                "after": updated.surfaces,
            }),
        );
    }
    if expires_at != key.expires_at {
        changes.insert(
            "expires_at".into(),
            serde_json::json!({
                "before": key.expires_at,
                "after": updated.expires_at,
            }),
        );
    }
    if req.rotation_period_days.is_some() && req.rotation_period_days != key.rotation_period_days {
        changes.insert(
            "rotation_period_days".into(),
            serde_json::json!({
                "before": key.rotation_period_days,
                "after": updated.rotation_period_days,
            }),
        );
    }
    if req.inactivity_timeout_days.is_some()
        && req.inactivity_timeout_days != key.inactivity_timeout_days
    {
        changes.insert(
            "inactivity_timeout_days".into(),
            serde_json::json!({
                "before": key.inactivity_timeout_days,
                "after": updated.inactivity_timeout_days,
            }),
        );
    }
    state.audit.log(
        AuditEntry::new("api_key.update")
            .user_id(auth_user.claims.sub)
            .resource(format!("api_key:{id}"))
            .detail(serde_json::Value::Object(changes)),
    );

    Ok(Json(updated))
}

/// POST /api/keys/{id}/rotate — rotate an API key, returning a new key.
#[utoipa::path(
    post,
    path = "/api/keys/{id}/rotate",
    tag = "API Keys",
    params(
        ("id" = Uuid, Path, description = "API key UUID to rotate"),
    ),
    responses(
        (status = 200, description = "New key generated; old key enters grace period"),
        (status = 400, description = "Key is inactive"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "API key not found"),
    ),
)]
#[tracing::instrument(skip_all, fields(handler = "api_keys.rotate_key"))]
pub async fn rotate_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<CreateApiKeyResponse>, AppError> {
    assert_owner_or_global(&auth_user, &state.db, "api_keys:rotate", id).await?;
    let old_key =
        sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound("API key not found".into()))?;

    if !old_key.is_active {
        return Err(AppError::BadRequest("Cannot rotate an inactive key".into()));
    }

    let grace_hours = state
        .dynamic_config
        .api_keys_rotation_grace_period_hours()
        .await;
    let grace_period_ends_at = chrono::Utc::now() + chrono::Duration::hours(grace_hours);

    // Generate new key. Rate-limit / budget rules attached to the
    // OLD key id are NOT copied here — they live in `rate_limit_rules`
    // / `budget_caps` keyed by api_key UUID. The follow-up phase
    // ("limits attached to api_keys") will add an explicit copy step
    // when rotation happens.
    let generated = api_key::generate_api_key();

    // INSERT new key + UPDATE old key's grace period must be atomic.
    // Without the transaction, an error between the two leaves the
    // old key with no grace_period_ends_at — meaning it never enters
    // the rotation grace window and both keys remain valid forever.
    let mut tx = state.db.begin().await?;

    let new_key = sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, surfaces, allowed_models,
            expires_at, rotation_period_days, inactivity_timeout_days,
            rotated_from_id, last_rotation_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
           RETURNING *"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(format!("{} (rotated)", old_key.name))
    .bind(old_key.user_id)
    .bind(&old_key.surfaces)
    .bind(&old_key.allowed_models)
    .bind(old_key.expires_at)
    .bind(old_key.rotation_period_days)
    .bind(old_key.inactivity_timeout_days)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE api_keys SET grace_period_ends_at = $1, disabled_reason = 'rotated' WHERE id = $2",
    )
    .bind(grace_period_ends_at)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    state.audit.log(
        AuditEntry::new("api_key.rotate")
            .user_id(auth_user.claims.sub)
            .resource(format!("api_key:{id}"))
            .detail(serde_json::json!({
                "new_key_id": new_key.id,
                "grace_period_ends_at": grace_period_ends_at.to_rfc3339(),
            })),
    );

    Ok(Json(CreateApiKeyResponse {
        id: new_key.id,
        key: generated.plaintext,
        name: new_key.name,
        key_prefix: new_key.key_prefix,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ExpiringKeysQuery {
    pub days: Option<i32>,
}

/// GET /api/keys/expiring — list keys expiring within N days.
///
/// Caller sees their own expiring keys; admins with `api_keys:read` at
/// global scope see everyone's. Soft-deleted keys are filtered out.
#[utoipa::path(
    get,
    path = "/api/keys/expiring",
    tag = "API Keys",
    params(
        ("days" = Option<i32>, Query, description = "Number of days to look ahead (default 7)"),
    ),
    responses(
        (status = 200, description = "List of API keys expiring within the specified window"),
        (status = 401, description = "Unauthorized"),
    ),
)]
pub async fn list_expiring_keys(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ExpiringKeysQuery>,
) -> Result<Json<Vec<ApiKey>>, AppError> {
    auth_user.require_permission("api_keys:read")?;
    let days = query.days.unwrap_or(7);
    let threshold = chrono::Utc::now() + chrono::Duration::days(days as i64);
    let caller_id = auth_user.claims.sub;

    let global = caller_has_global(&auth_user, &state.db, "api_keys:read").await?;

    let keys = if global {
        sqlx::query_as::<_, ApiKey>(
            r#"SELECT * FROM api_keys
               WHERE is_active = true
                 AND deleted_at IS NULL
                 AND expires_at IS NOT NULL
                 AND expires_at <= $1
               ORDER BY expires_at ASC"#,
        )
        .bind(threshold)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, ApiKey>(
            r#"SELECT * FROM api_keys
               WHERE is_active = true
                 AND deleted_at IS NULL
                 AND expires_at IS NOT NULL
                 AND expires_at <= $1
                 AND user_id = $2
               ORDER BY expires_at ASC"#,
        )
        .bind(threshold)
        .bind(caller_id)
        .fetch_all(&state.db)
        .await?
    };

    Ok(Json(keys))
}

/// Validate a cost_center value: trim whitespace, reject if > 64 chars,
/// convert empty string to None. Shared by create_key and update_key so
/// both paths apply the same normalisation.
fn validate_cost_center(input: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = input else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > 64 {
        return Err(AppError::BadRequest(
            "cost_center must be 64 characters or fewer".into(),
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

/// GET /api/keys/cost-centers — distinct non-null tags currently in use,
/// sorted alphabetically. Used by the admin UI to autocomplete the
/// cost-center field on the api-key create / edit forms.
#[utoipa::path(
    get,
    path = "/api/keys/cost-centers",
    tag = "API Keys",
    responses(
        (status = 200, description = "Distinct cost-center tags in use", body = Vec<String>),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_cost_centers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, AppError> {
    auth_user.require_permission("api_keys:read")?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT cost_center FROM api_keys \
          WHERE cost_center IS NOT NULL AND deleted_at IS NULL \
          ORDER BY cost_center ASC",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(|(s,)| s).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cost_center_accepts_valid_input() {
        assert_eq!(validate_cost_center(None).unwrap(), None);
        assert_eq!(validate_cost_center(Some("")).unwrap(), None);
        assert_eq!(validate_cost_center(Some("   ")).unwrap(), None);
        assert_eq!(
            validate_cost_center(Some(" team-alpha "))
                .unwrap()
                .as_deref(),
            Some("team-alpha"),
        );
    }

    #[test]
    fn validate_cost_center_rejects_too_long() {
        let too_long = "x".repeat(65);
        assert!(validate_cost_center(Some(&too_long)).is_err());
        // Exactly 64 is fine.
        let ok = "x".repeat(64);
        assert!(validate_cost_center(Some(&ok)).is_ok());
    }

    #[test]
    fn validate_cost_center_counts_chars_not_bytes() {
        // 64 4-byte CJK chars should pass (unicode length, not byte length).
        let cjk = "部".repeat(64);
        assert!(validate_cost_center(Some(&cjk)).is_ok());
        let cjk65 = "部".repeat(65);
        assert!(validate_cost_center(Some(&cjk65)).is_err());
    }
}
