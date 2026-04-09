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
use sqlx::FromRow;
use uuid::Uuid;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct TeamWithCount {
    #[serde(flatten)]
    pub team: Team,
    pub member_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTeamRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TeamMemberRow {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String, // 'member' | 'manager' on the team_members row
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    pub user_id: Uuid,
    /// 'member' (default) or 'manager'. The team_members.role
    /// column is informational — actual platform-level access
    /// comes from RBAC role assignments scoped to the team.
    #[serde(default)]
    pub role: Option<String>,
}

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
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM team_members WHERE user_id = $1 AND team_id = $2)",
    )
    .bind(caller_id)
    .bind(team_id)
    .fetch_one(pool)
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
pub async fn list_teams(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<TeamWithCount>>, AppError> {
    auth_user.require_permission("teams:read")?;

    // Determine if caller has global read scope.
    let global = auth_user
        .owned_team_scope_for_perm(&state.db, "teams:read")
        .await?
        .is_none();

    let rows: Vec<TeamWithCount> = if global {
        sqlx::query_as::<_, TeamWithCountRow>(
            "SELECT t.id, t.name, t.description, t.created_at, \
                    COALESCE(c.cnt, 0) AS member_count \
               FROM teams t \
          LEFT JOIN ( \
               SELECT team_id, COUNT(*) AS cnt FROM team_members GROUP BY team_id \
          ) c ON c.team_id = t.id \
              ORDER BY t.name ASC",
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(Into::into)
        .collect()
    } else {
        sqlx::query_as::<_, TeamWithCountRow>(
            "SELECT t.id, t.name, t.description, t.created_at, \
                    COALESCE(c.cnt, 0) AS member_count \
               FROM teams t \
               JOIN team_members tm ON tm.team_id = t.id \
          LEFT JOIN ( \
               SELECT team_id, COUNT(*) AS cnt FROM team_members GROUP BY team_id \
          ) c ON c.team_id = t.id \
              WHERE tm.user_id = $1 \
              ORDER BY t.name ASC",
        )
        .bind(auth_user.claims.sub)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(Into::into)
        .collect()
    };

    Ok(Json(rows))
}

#[derive(FromRow)]
struct TeamWithCountRow {
    id: Uuid,
    name: String,
    description: Option<String>,
    created_at: DateTime<Utc>,
    member_count: i64,
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
pub async fn get_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Team>, AppError> {
    auth_user.require_permission("teams:read")?;
    assert_can_view_team(&auth_user, &state.db, id).await?;
    let team = sqlx::query_as::<_, Team>(
        "SELECT id, name, description, created_at FROM teams WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Team not found".into()))?;
    Ok(Json(team))
}

/// POST /api/admin/teams
pub async fn create_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTeamRequest>,
) -> Result<Json<Team>, AppError> {
    auth_user.require_permission("teams:create")?;
    auth_user
        .assert_scope_global(&state.db, "teams:create")
        .await?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Team name is required".into()));
    }
    if name.len() > 255 {
        return Err(AppError::BadRequest("Team name too long".into()));
    }
    let team = sqlx::query_as::<_, Team>(
        "INSERT INTO teams (name, description) VALUES ($1, $2) \
         RETURNING id, name, description, created_at",
    )
    .bind(name)
    .bind(req.description.as_deref().map(str::trim))
    .fetch_one(&state.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
            AppError::Conflict(format!("Team '{name}' already exists"))
        }
        other => AppError::Internal(anyhow::anyhow!("create team failed: {other}")),
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

    let existing = sqlx::query_as::<_, Team>(
        "SELECT id, name, description, created_at FROM teams WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Team not found".into()))?;

    let new_name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.name);
    let new_desc = req
        .description
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| existing.description.clone());

    let updated = sqlx::query_as::<_, Team>(
        "UPDATE teams SET name = $2, description = $3 WHERE id = $1 \
         RETURNING id, name, description, created_at",
    )
    .bind(id)
    .bind(new_name)
    .bind(new_desc)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
            AppError::Conflict(format!("Team '{new_name}' already exists"))
        }
        other => AppError::Internal(anyhow::anyhow!("update team failed: {other}")),
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
/// Deleting a team CASCADEs to:
///   - team_members entries (FK)
///   - rbac_role_assignments scoped to this team (FK we added in
///     phase 0)
///   - budget_caps and rate_limit_rules subjects keyed to this
///     team will be left orphaned, but the data retention sweep
///     scrubs those daily (see tasks/data_retention.rs).
pub async fn delete_team(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("teams:delete")?;
    auth_user
        .assert_scope_global(&state.db, "teams:delete")
        .await?;

    let name: Option<String> = sqlx::query_scalar("SELECT name FROM teams WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let name = name.ok_or_else(|| AppError::NotFound("Team not found".into()))?;

    sqlx::query("DELETE FROM teams WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

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

    type Row = (Uuid, String, String, String, DateTime<Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, tm.role, tm.joined_at \
           FROM team_members tm \
           JOIN users u ON u.id = tm.user_id \
          WHERE tm.team_id = $1 \
            AND u.deleted_at IS NULL \
          ORDER BY tm.joined_at ASC",
    )
    .bind(team_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(user_id, email, display_name, role, joined_at)| TeamMemberRow {
                    user_id,
                    email,
                    display_name,
                    role,
                    joined_at,
                },
            )
            .collect(),
    ))
}

/// POST /api/admin/teams/{id}/members
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

    // Validate the user actually exists and isn't soft-deleted.
    let user_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(req.user_id)
    .fetch_one(&state.db)
    .await?;
    if !user_exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    let role = req.role.as_deref().unwrap_or("member");
    if !matches!(role, "member" | "manager") {
        return Err(AppError::BadRequest(
            "role must be 'member' or 'manager'".into(),
        ));
    }

    sqlx::query(
        "INSERT INTO team_members (user_id, team_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, team_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(req.user_id)
    .bind(team_id)
    .bind(role)
    .execute(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("team_member.add")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "user_id": req.user_id, "role": role })),
    );

    Ok(Json(serde_json::json!({"status": "added"})))
}

/// DELETE /api/admin/teams/{id}/members/{user_id}
pub async fn remove_member(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("team_members:write")?;
    auth_user
        .assert_scope_for_team(&state.db, "team_members:write", team_id)
        .await?;

    let removed = sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
        .bind(team_id)
        .bind(user_id)
        .execute(&state.db)
        .await?
        .rows_affected();

    if removed == 0 {
        return Err(AppError::NotFound("Member not found".into()));
    }

    state.audit.log(
        auth_user
            .audit("team_member.remove")
            .resource("team")
            .resource_id(team_id.to_string())
            .detail(serde_json::json!({ "user_id": user_id })),
    );

    Ok(Json(serde_json::json!({"status": "removed"})))
}
