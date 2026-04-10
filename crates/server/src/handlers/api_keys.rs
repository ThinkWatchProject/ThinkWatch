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

/// GET /api/keys
///
/// Result set is the union of:
///   - the caller's own keys (always)
///   - keys belonging to users in any team the caller has
///     `api_keys:read` scoped to (team_manager case)
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
    let per_page = pagination.per_page();
    let offset = pagination.offset();
    let caller_id = auth_user.claims.sub;

    // Resolve the visible-key filter once. None = global, Some(set)
    // = caller can only see keys belonging to users in those teams
    // (plus their own).
    let owned_teams = auth_user
        .owned_team_scope_for_perm(&state.db, "api_keys:read")
        .await?;

    let (total, keys): (i64, Vec<ApiKey>) = match owned_teams {
        None => {
            // Global scope — no filter at all.
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
        }
        Some(team_ids) => {
            // Caller can see their own keys plus any key whose owner
            // is in one of the team_ids set.
            let team_ids_vec: Vec<Uuid> = team_ids.into_iter().collect();
            let total: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM api_keys k \
                  WHERE k.deleted_at IS NULL \
                    AND ( \
                        k.user_id = $1 \
                        OR EXISTS ( \
                            SELECT 1 FROM team_members tm \
                             WHERE tm.user_id = k.user_id \
                               AND tm.team_id = ANY($2) \
                        ) \
                    )",
            )
            .bind(caller_id)
            .bind(&team_ids_vec)
            .fetch_one(&state.db)
            .await?;
            let keys = sqlx::query_as::<_, ApiKey>(
                "SELECT k.* FROM api_keys k \
                  WHERE k.deleted_at IS NULL \
                    AND ( \
                        k.user_id = $1 \
                        OR EXISTS ( \
                            SELECT 1 FROM team_members tm \
                             WHERE tm.user_id = k.user_id \
                               AND tm.team_id = ANY($2) \
                        ) \
                    ) \
                  ORDER BY k.created_at DESC LIMIT $3 OFFSET $4",
            )
            .bind(caller_id)
            .bind(&team_ids_vec)
            .bind(per_page as i64)
            .bind(offset as i64)
            .fetch_all(&state.db)
            .await?;
            (total, keys)
        }
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
                "Unknown surface '{s}' (allowed: ai_gateway, mcp_gateway)"
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
        description = "Key name, surfaces, optional team_id, models, and expiry",
    ),
    responses(
        (status = 200, description = "API key created — plaintext key shown only once"),
        (status = 400, description = "Invalid surfaces or team membership"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Cannot create keys for other teams without team:write"),
    ),
)]
pub async fn create_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>, AppError> {
    let surfaces = normalize_surfaces(&req.surfaces)?;

    // Validate team membership if team_id is specified
    if let Some(team_id) = req.team_id {
        let is_member = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM team_members WHERE user_id = $1 AND team_id = $2)",
        )
        .bind(auth_user.claims.sub)
        .bind(team_id)
        .fetch_one(&state.db)
        .await?;

        if !is_member {
            // Allow users with cross-team API key management to create
            // keys for any team. The `api_keys:create` permission alone
            // only grants access to the caller's own team; creating for
            // another team requires full API-key administration rights,
            // which we express as `team:write`.
            auth_user
                .require_permission("team:write")
                .map_err(|_| AppError::Forbidden("Cannot create keys for other teams".into()))?;
        }
    }

    let generated = api_key::generate_api_key();

    let expires_at = req
        .expires_in_days
        .map(|days| chrono::Utc::now() + chrono::Duration::days(days as i64));

    let row = sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, team_id, surfaces, allowed_models, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(&req.name)
    .bind(auth_user.claims.sub)
    .bind(req.team_id)
    .bind(&surfaces)
    .bind(&req.allowed_models)
    .bind(expires_at)
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
        (status = 403, description = "Forbidden — key is outside the caller's team scope"),
        (status = 404, description = "API key not found"),
    ),
)]
pub async fn get_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApiKey>, AppError> {
    // Scope check first — bounces requests for keys outside the
    // caller's owned team set with a 403 instead of leaking the
    // key's existence via a "not found" probe.
    auth_user
        .assert_scope_for_api_key(&state.db, "api_keys:read", id)
        .await?;
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
pub async fn revoke_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .assert_scope_for_api_key(&state.db, "api_keys:delete", id)
        .await?;

    let result = sqlx::query(
        "UPDATE api_keys SET is_active = false, disabled_reason = 'revoked' \
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

// --- API key lifecycle management ---

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateKeyRequest {
    pub allowed_models: Option<Vec<String>>,
    /// When `Some`, replaces the entire surfaces list. Must still
    /// be non-empty. Omit the field to leave surfaces untouched.
    pub surfaces: Option<Vec<String>>,
    pub expires_in_days: Option<i32>,
    pub rotation_period_days: Option<i32>,
    pub inactivity_timeout_days: Option<i32>,
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
    auth_user
        .assert_scope_for_api_key(&state.db, "api_keys:update", id)
        .await?;
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

    let updated = sqlx::query_as::<_, ApiKey>(
        r#"UPDATE api_keys SET
            allowed_models = COALESCE($1, allowed_models),
            surfaces = COALESCE($2, surfaces),
            expires_at = $3,
            rotation_period_days = COALESCE($4, rotation_period_days),
            inactivity_timeout_days = COALESCE($5, inactivity_timeout_days)
           WHERE id = $6 RETURNING *"#,
    )
    .bind(&req.allowed_models)
    .bind(normalized_surfaces.as_ref())
    .bind(expires_at)
    .bind(req.rotation_period_days)
    .bind(req.inactivity_timeout_days)
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        AuditEntry::new("api_key.update")
            .user_id(auth_user.claims.sub)
            .resource(format!("api_key:{id}")),
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
pub async fn rotate_key(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<CreateApiKeyResponse>, AppError> {
    auth_user
        .assert_scope_for_api_key(&state.db, "api_keys:rotate", id)
        .await?;
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
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, team_id, surfaces, allowed_models,
            expires_at, rotation_period_days, inactivity_timeout_days,
            rotated_from_id, last_rotation_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
           RETURNING *"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(format!("{} (rotated)", old_key.name))
    .bind(old_key.user_id)
    .bind(old_key.team_id)
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
/// Always includes the caller's own expiring keys, plus any keys
/// belonging to users in teams the caller has `api_keys:read` for
/// (so a team manager sees the whole team's expiring set, not just
/// their own). Soft-deleted keys are filtered out.
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
    let days = query.days.unwrap_or(7);
    let threshold = chrono::Utc::now() + chrono::Duration::days(days as i64);
    let caller_id = auth_user.claims.sub;

    let owned_teams = auth_user
        .owned_team_scope_for_perm(&state.db, "api_keys:read")
        .await?;

    let keys = match owned_teams {
        None => {
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
        }
        Some(team_ids) => {
            let team_ids_vec: Vec<Uuid> = team_ids.into_iter().collect();
            sqlx::query_as::<_, ApiKey>(
                r#"SELECT k.* FROM api_keys k
                   WHERE k.is_active = true
                     AND k.deleted_at IS NULL
                     AND k.expires_at IS NOT NULL
                     AND k.expires_at <= $1
                     AND (
                         k.user_id = $2
                         OR EXISTS (
                             SELECT 1 FROM team_members tm
                              WHERE tm.user_id = k.user_id
                                AND tm.team_id = ANY($3)
                         )
                     )
                   ORDER BY k.expires_at ASC"#,
            )
            .bind(threshold)
            .bind(caller_id)
            .bind(&team_ids_vec)
            .fetch_all(&state.db)
            .await?
        }
    };

    Ok(Json(keys))
}
