// ============================================================================
// Teams CRUD + membership management
//
// Two related resources:
//
//   teams         — the team catalog itself (rename, list, delete)
//   team_members  — (user_id, team_id) memberships
//
// Auth model:
//   - `teams:read` — list / get a team's metadata + member list.
//     Required at GLOBAL scope to list ALL teams. Members of a
//     team can ALSO read their own team's metadata + roster as a
//     baseline knowledge right (no perm needed) — see
//     `caller_can_view_team`.
//   - `teams:create` / `teams:update` / `teams:delete` — global
//     only. Mutating the team catalog is platform-wide bookkeeping.
//   - `team_members:write` — add/remove members. Must hold the
//     perm at global scope OR at scope_kind=team for the specific
//     team being mutated. Lets a team_manager onboard members
//     into their own team without granting them permission to
//     touch other teams.
//
// Scoped role assignments are written by the existing
// `write_user_role_assignments` helper in admin.rs — this file
// only manages the team catalog and the member rows.
// ============================================================================

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;

use super::serde_util::deserialize_some;
use crate::app::AppState;
use crate::middleware::auth_guard::{AuthUser, invalidate_team_perms, invalidate_user_perms};
use crate::services::team_repository::{self as repo, Team, TeamRoleRow, TeamWithCountRow};
use crate::services::user_repository;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TeamWithCount {
    #[serde(flatten)]
    pub team: Team,
    pub member_count: i64,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateTeamRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateTeamRequest {
    pub name: Option<String>,
    /// PATCH semantics: absent = unchanged, JSON `null` = clear,
    /// JSON string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TeamMemberRow {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AddMemberRequest {
    pub user_id: Uuid,
}

/// Maximum number of teams a single user can belong to.
const MAX_TEAMS_PER_USER: i64 = 10;

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// Returns true if the caller is a member of `team_id`. Used to
/// gate the "see your own team" baseline read right.
async fn caller_is_team_member(
    pool: &sqlx::PgPool,
    caller_id: Uuid,
    team_id: Uuid,
) -> Result<bool, AppError> {
    let exists = repo::is_member(pool, caller_id, team_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("team membership check failed: {e}")))?;
    Ok(exists)
}

/// Allow a team read if caller has `teams:read` globally OR is a
/// member of the team in question. Members of a team always have
/// read access to their own team's metadata + roster.
async fn assert_can_view_team(
    auth_user: &AuthUser,
    pool: &sqlx::PgPool,
    team_id: Uuid,
) -> Result<(), AppError> {
    if caller_is_team_member(pool, auth_user.claims.sub, team_id).await? {
        return Ok(());
    }
    auth_user.assert_scope_global(pool, "teams:read").await
}

// ----------------------------------------------------------------------------
// Teams CRUD
// ----------------------------------------------------------------------------

/// GET /api/admin/teams
///
/// Returns every team the caller can see:
///   - global `teams:read` → all teams
///   - otherwise → teams the caller is a member of
#[utoipa::path(
    get,
    path = "/api/admin/teams",
    tag = "Teams",
    responses(
        (status = 200, description = "List of teams with member counts"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_teams(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<TeamWithCount>>, AppError> {
    auth_user.require_permission("teams:read")?;

    // Visible team set:
    //   - Global `teams:read` scope → every team.
    //   - Otherwise the union of (a) teams the caller is a MEMBER of
    //     and (b) teams the caller has `teams:read` scoped TO via a
    //     direct or team-inherited role assignment. Previously only
    //     (a) was returned, so a user granted scoped `teams:read` for
    //     team X (without membership in X) could `GET /teams/{X}`
    //     successfully but couldn't see X in the listing — confusing
    //     UX and a real visibility bug for org-admin shapes.
    let scope = auth_user
        .owned_team_scope_for_perm(&state.db, "teams:read")
        .await?;

    let rows: Vec<TeamWithCount> = match scope {
        None => repo::list(&state.db)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
        Some(scoped_team_ids) => {
            // Convert the HashSet to a Vec for binding to ANY($2).
            let scoped: Vec<uuid::Uuid> = scoped_team_ids.iter().copied().collect();
            repo::list_for_member_or_in(&state.db, auth_user.claims.sub, &scoped)
                .await?
                .into_iter()
                .map(Into::into)
                .collect()
        }
    };

    Ok(Json(rows))
}

impl From<TeamWithCountRow> for TeamWithCount {
    fn from(r: TeamWithCountRow) -> Self {
        TeamWithCount {
            team: Team {
                id: r.id,
                name: r.name,
                description: r.description,
                created_at: r.created_at,
            },
            member_count: r.member_count,
        }
    }
}

/// GET /api/admin/teams/{id}
#[utoipa::path(
    get,
    path = "/api/admin/teams/{id}",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
    ),
    responses(
        (status = 200, description = "Team details", body = TeamWithCount),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Team not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TeamWithCount>, AppError> {
    auth_user.require_permission("teams:read")?;
    assert_can_view_team(&auth_user, &state.db, id).await?;
    // Same shape the list endpoint returns — frontend types both as
    // `Team` (which declares member_count), and the detail page
    // renders the count card off this field. Returning a bare Team
    // here left the card showing undefined and any optimistic
    // decrement after a member removal flipping to NaN.
    let row = repo::find_with_count(&state.db, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Team not found".into()))?;
    Ok(Json(row.into()))
}

/// POST /api/admin/teams
#[utoipa::path(
    post,
    path = "/api/admin/teams",
    tag = "Teams",
    request_body = CreateTeamRequest,
    responses(
        (status = 200, description = "Created team", body = Team),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 409, description = "Team name already exists"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn create_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTeamRequest>,
) -> Result<Json<Team>, AppError> {
    auth_user
        .require_global_permission(&state.db, "teams:create")
        .await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Team name is required".into()));
    }
    // Count Unicode scalars, not bytes — `.len()` would reject a
    // 64-character Chinese name as "too long" because each codepoint
    // is 3 bytes (CJK Unified). The UI error message says "characters",
    // so the bound should match user expectation.
    if name.chars().count() > 255 {
        return Err(AppError::BadRequest("Team name too long".into()));
    }
    let team = repo::insert(&state.db, name, req.description.as_deref().map(str::trim))
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                AppError::Conflict(format!("Team '{name}' already exists"))
            }
            // Delegate non-unique-violation errors to the global
            // `From<sqlx::Error>` mapping. Without `e.into()`, the
            // catch-all `Internal(...)` here would swallow the
            // `PoolTimedOut` / `PoolClosed` / `WorkerCrashed` / `Io`
            // → `ServiceUnavailable(503)` distinction the global
            // mapping makes, losing operator-facing infra-vs-app
            // separation in dashboards.
            other => other.into(),
        })?;

    state.audit.log(
        auth_user
            .audit("team.create")
            .resource("team")
            .resource_id(team.id.to_string())
            .detail(serde_json::json!({ "name": team.name })),
    );

    Ok(Json(team))
}

/// PATCH /api/admin/teams/{id}
#[utoipa::path(
    patch,
    path = "/api/admin/teams/{id}",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
    ),
    request_body = UpdateTeamRequest,
    responses(
        (status = 200, description = "Updated team", body = Team),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Team not found"),
        (status = 409, description = "Team name already exists"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn update_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateTeamRequest>,
) -> Result<Json<Team>, AppError> {
    auth_user.require_permission("teams:update")?;
    // Update accepts either global scope OR scope to this specific team
    // (so a team manager can rename their own team if granted teams:update).
    auth_user
        .assert_scope_for_team(&state.db, "teams:update", id)
        .await?;

    let existing = repo::find(&state.db, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Team not found".into()))?;

    // Distinguish absent (preserve current) from empty (reject).
    // The previous shape silently fell back to `existing.name` on
    // whitespace input — so a client sending `{ "name": "   " }`
    // saw a 200 OK with the name unchanged, which is the opposite of
    // create_team's strict rejection. Now consistent with create_team:
    // absent field → preserve, present-but-empty → 400.
    let new_name = match req.name.as_deref().map(str::trim) {
        None => existing.name.as_str(),
        Some("") => {
            return Err(AppError::BadRequest(
                "Team name cannot be empty or whitespace".into(),
            ));
        }
        Some(s) => s,
    };
    // None = absent (preserve), Some(None) = clear, Some(Some(s)) = set.
    // Treat empty / whitespace-only strings as "clear" for parity with
    // the previous unwrap-empty-then-fallback behaviour the UI relied
    // on; the API itself still distinguishes the three states.
    let new_desc: Option<String> = match &req.description {
        None => existing.description.clone(),
        Some(None) => None,
        Some(Some(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    };

    let updated = repo::update(&state.db, id, new_name, new_desc)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                AppError::Conflict(format!("Team '{new_name}' already exists"))
            }
            // See create_team above — delegate to global mapping so
            // transient-vs-permanent DB failures stay distinguishable.
            other => other.into(),
        })?;

    state.audit.log(
        auth_user
            .audit("team.update")
            .resource("team")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": new_name })),
    );

    Ok(Json(updated))
}

/// DELETE /api/admin/teams/{id}
///
/// Deleting a team CASCADEs to team_members, rbac_role_assignments, etc.
#[utoipa::path(
    delete,
    path = "/api/admin/teams/{id}",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
    ),
    responses(
        (status = 200, description = "Team deleted"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Team not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn delete_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "teams:delete")
        .await?;

    let name = repo::name_of(&state.db, id).await?;
    let name = name.ok_or_else(|| AppError::NotFound("Team not found".into()))?;

    repo::delete(&state.db, id).await?;

    state.audit.log(
        auth_user
            .audit("team.delete")
            .resource("team")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ----------------------------------------------------------------------------
// Members
// ----------------------------------------------------------------------------

/// GET /api/admin/teams/{id}/members
#[utoipa::path(
    get,
    path = "/api/admin/teams/{id}/members",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
    ),
    responses(
        (status = 200, description = "Team member list"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Team not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_members(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(team_id): Path<Uuid>,
) -> Result<Json<Vec<TeamMemberRow>>, AppError> {
    // Members of a team always see their own team's roster (a
    // baseline knowledge right, not a permission). Outside that,
    // require teams:read at any scope.
    if !caller_is_team_member(&state.db, auth_user.claims.sub, team_id).await? {
        auth_user.require_permission("teams:read")?;
        auth_user
            .assert_scope_for_team(&state.db, "teams:read", team_id)
            .await?;
    }

    let rows = repo::members(&state.db, team_id).await?;

    Ok(Json(
        rows.into_iter()
            .map(|(user_id, email, display_name, joined_at)| TeamMemberRow {
                user_id,
                email,
                display_name,
                joined_at,
            })
            .collect(),
    ))
}

/// POST /api/admin/teams/{id}/members
#[utoipa::path(
    post,
    path = "/api/admin/teams/{id}/members",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
    ),
    request_body = AddMemberRequest,
    responses(
        (status = 200, description = "Member added"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "User not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn add_member(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(team_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("team_members:write")?;
    auth_user
        .assert_scope_for_team(&state.db, "team_members:write", team_id)
        .await?;

    // Validate the user actually exists, is active, and isn't soft-deleted.
    let user_exists = user_repository::active_exists(&state.db, req.user_id).await?;
    if !user_exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    // Atomic max-teams check: INSERT only if the user currently belongs
    // to fewer than MAX teams. Prior code did SELECT COUNT + INSERT as
    // two separate statements, letting two concurrent requests race past
    // the limit. The subquery runs against the committed snapshot but
    // INSERT's row-level locking ensures only one concurrent inserter
    // can succeed — the other's subquery will re-evaluate after the
    // first commits (PG read-committed semantics). We verify the insert
    // actually happened by `rows_affected`.
    //
    // ON CONFLICT DO NOTHING handles re-adding an existing member
    // idempotently (0 rows affected but not an error).
    let inserted =
        repo::add_member_capped(&state.db, req.user_id, team_id, MAX_TEAMS_PER_USER).await?;

    // 0 rows can mean "already a member" (fine) OR "at limit" (error).
    // Disambiguate with a follow-up check so we return the right message.
    if inserted == 0 {
        let already_member = repo::is_member(&state.db, req.user_id, team_id).await?;
        if !already_member {
            return Err(AppError::BadRequest(format!(
                "User already belongs to {MAX_TEAMS_PER_USER} teams (maximum)"
            )));
        }
    }

    invalidate_user_perms(&state.redis, req.user_id).await;

    state.audit.log(
        auth_user
            .audit("team_member.add")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "user_id": req.user_id })),
    );

    Ok(Json(serde_json::json!({"status": "added"})))
}

/// DELETE /api/admin/teams/{id}/members/{user_id}
#[utoipa::path(
    delete,
    path = "/api/admin/teams/{id}/members/{user_id}",
    tag = "Teams",
    params(
        ("id" = uuid::Uuid, Path, description = "Team ID"),
        ("user_id" = uuid::Uuid, Path, description = "User ID to remove"),
    ),
    responses(
        (status = 200, description = "Member removed"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Member not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn remove_member(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("team_members:write")?;
    auth_user
        .assert_scope_for_team(&state.db, "team_members:write", team_id)
        .await?;

    let removed = repo::remove_member(&state.db, team_id, user_id).await?;

    if removed == 0 {
        return Err(AppError::NotFound("Member not found".into()));
    }

    invalidate_user_perms(&state.redis, user_id).await;

    state.audit.log(
        auth_user
            .audit("team_member.remove")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "user_id": user_id })),
    );

    Ok(Json(serde_json::json!({"status": "removed"})))
}

// ---------------------------------------------------------------------------
// Team role assignments — roles assigned to a team are inherited by all
// members. This turns teams into permission groups.
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/teams/{id}/roles",
    tag = "Teams",
    params(("id" = Uuid, Path, description = "Team ID")),
    responses(
        (status = 200, description = "List of roles assigned to team"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_team_roles(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(team_id): Path<Uuid>,
) -> Result<Json<Vec<TeamRoleRow>>, AppError> {
    auth_user.require_permission("teams:read")?;
    auth_user
        .assert_scope_for_team(&state.db, "teams:read", team_id)
        .await?;

    let rows = repo::roles(&state.db, team_id).await?;

    Ok(Json(rows))
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct AssignTeamRoleRequest {
    pub role_id: Uuid,
}

#[utoipa::path(
    post,
    path = "/api/admin/teams/{id}/roles",
    tag = "Teams",
    params(("id" = Uuid, Path, description = "Team ID")),
    request_body = AssignTeamRoleRequest,
    responses(
        (status = 200, description = "Role assigned to team"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn assign_team_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(team_id): Path<Uuid>,
    Json(req): Json<AssignTeamRoleRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("teams:update")?;
    auth_user
        .assert_scope_for_team(&state.db, "teams:update", team_id)
        .await?;

    repo::assign_role(&state.db, team_id, req.role_id, auth_user.claims.sub).await?;

    invalidate_team_perms(&state.db, &state.redis, team_id).await;

    state.audit.log(
        auth_user
            .audit("team_role.assigned")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "role_id": req.role_id })),
    );

    Ok(Json(serde_json::json!({"status": "assigned"})))
}

#[utoipa::path(
    delete,
    path = "/api/admin/teams/{id}/roles/{role_id}",
    tag = "Teams",
    params(
        ("id" = Uuid, Path, description = "Team ID"),
        ("role_id" = Uuid, Path, description = "Role ID"),
    ),
    responses(
        (status = 200, description = "Role removed from team"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn remove_team_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((team_id, role_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("teams:update")?;
    auth_user
        .assert_scope_for_team(&state.db, "teams:update", team_id)
        .await?;

    repo::remove_role(&state.db, team_id, role_id).await?;

    invalidate_team_perms(&state.db, &state.redis, team_id).await;

    state.audit.log(
        auth_user
            .audit("team_role.removed")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "role_id": role_id })),
    );

    Ok(Json(serde_json::json!({"status": "removed"})))
}
