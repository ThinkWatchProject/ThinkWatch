//! Team repository — the `teams` catalog, `team_members` and
//! `team_role_assignments`.
//!
//! Thin wrappers over sqlx, one statement per function; permission
//! checks, name validation, audit and permission-cache invalidation stay
//! in `handlers::teams`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use think_watch_common::errors::AppError;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, utoipa::ToSchema)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A team with its member count joined in.
#[derive(FromRow)]
pub struct TeamWithCountRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub member_count: i64,
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct TeamRoleRow {
    pub role_id: Uuid,
    pub name: String,
    pub is_system: bool,
    pub assigned_at: chrono::DateTime<chrono::Utc>,
}

/// One roster entry: (user id, email, display name, joined_at).
pub type TeamMemberTuple = (Uuid, String, String, DateTime<Utc>);

// ---------------------------------------------------------------------------
// teams
// ---------------------------------------------------------------------------

/// Every team, by name.
pub async fn list(pool: &PgPool) -> Result<Vec<TeamWithCountRow>, AppError> {
    Ok(sqlx::query_as::<_, TeamWithCountRow>(
        "SELECT t.id, t.name, t.description, t.created_at, \
                COALESCE(c.cnt, 0) AS member_count \
           FROM teams t \
      LEFT JOIN ( \
           SELECT team_id, COUNT(*) AS cnt FROM team_members GROUP BY team_id \
      ) c ON c.team_id = t.id \
          ORDER BY t.name ASC",
    )
    .fetch_all(pool)
    .await?)
}

/// The teams `user_id` belongs to, plus `team_ids`, by name.
pub async fn list_for_member_or_in(
    pool: &PgPool,
    user_id: Uuid,
    team_ids: &[Uuid],
) -> Result<Vec<TeamWithCountRow>, AppError> {
    Ok(sqlx::query_as::<_, TeamWithCountRow>(
        "SELECT t.id, t.name, t.description, t.created_at, \
                COALESCE(c.cnt, 0) AS member_count \
           FROM teams t \
      LEFT JOIN ( \
           SELECT team_id, COUNT(*) AS cnt FROM team_members GROUP BY team_id \
      ) c ON c.team_id = t.id \
          WHERE EXISTS ( \
              SELECT 1 FROM team_members tm \
               WHERE tm.team_id = t.id AND tm.user_id = $1 \
          ) OR t.id = ANY($2) \
          ORDER BY t.name ASC",
    )
    .bind(user_id)
    .bind(team_ids)
    .fetch_all(pool)
    .await?)
}

pub async fn find_with_count(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<TeamWithCountRow>, AppError> {
    Ok(sqlx::query_as::<_, TeamWithCountRow>(
        "SELECT t.id, t.name, t.description, t.created_at, \
                COALESCE(c.cnt, 0) AS member_count \
           FROM teams t \
           LEFT JOIN (SELECT team_id, COUNT(*) AS cnt \
                        FROM team_members GROUP BY team_id) c \
                  ON c.team_id = t.id \
          WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Team>, AppError> {
    Ok(sqlx::query_as::<_, Team>(
        "SELECT id, name, description, created_at FROM teams WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn name_of(pool: &PgPool, id: Uuid) -> Result<Option<String>, AppError> {
    Ok(sqlx::query_scalar("SELECT name FROM teams WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Create a team. The raw error comes back so the caller can report a
/// taken name.
pub async fn insert(
    pool: &PgPool,
    name: &str,
    description: Option<&str>,
) -> Result<Team, sqlx::Error> {
    sqlx::query_as::<_, Team>(
        "INSERT INTO teams (name, description) VALUES ($1, $2) \
         RETURNING id, name, description, created_at",
    )
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Rename / re-describe a team. The raw error comes back so the caller
/// can report a taken name.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    description: Option<String>,
) -> Result<Team, sqlx::Error> {
    sqlx::query_as::<_, Team>(
        "UPDATE teams SET name = $2, description = $3 WHERE id = $1 \
         RETURNING id, name, description, created_at",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Delete a team; memberships and team role assignments cascade.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM teams WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// team_members
// ---------------------------------------------------------------------------

/// Is `user_id` a member of `team_id`? The raw error comes back so the
/// membership gate can say which check failed.
pub async fn is_member(pool: &PgPool, user_id: Uuid, team_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM team_members WHERE user_id = $1 AND team_id = $2)",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_one(pool)
    .await
}

/// A team's live members, in join order.
pub async fn members(pool: &PgPool, team_id: Uuid) -> Result<Vec<TeamMemberTuple>, AppError> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, tm.joined_at \
           FROM team_members tm \
           JOIN users u ON u.id = tm.user_id \
          WHERE tm.team_id = $1 \
            AND u.deleted_at IS NULL \
          ORDER BY tm.joined_at ASC",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?)
}

/// Add a member, but only while the user belongs to fewer than
/// `max_teams` teams; an existing membership is left alone. Returns the
/// rows inserted (0 = already a member, or at the cap).
///
/// The cap check and the insert are one statement so two concurrent
/// adds can't both slip past the limit.
pub async fn add_member_capped(
    pool: &PgPool,
    user_id: Uuid,
    team_id: Uuid,
    max_teams: i64,
) -> Result<u64, AppError> {
    Ok(sqlx::query(
        r#"INSERT INTO team_members (user_id, team_id)
           SELECT $1, $2
           WHERE (SELECT COUNT(*) FROM team_members WHERE user_id = $1) < $3
           ON CONFLICT (user_id, team_id) DO NOTHING"#,
    )
    .bind(user_id)
    .bind(team_id)
    .bind(max_teams)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Returns the rows deleted.
pub async fn remove_member(pool: &PgPool, team_id: Uuid, user_id: Uuid) -> Result<u64, AppError> {
    Ok(
        sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_id)
            .bind(user_id)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

// ---------------------------------------------------------------------------
// team_role_assignments
// ---------------------------------------------------------------------------

/// A team's roles, system roles first, then by name.
pub async fn roles(pool: &PgPool, team_id: Uuid) -> Result<Vec<TeamRoleRow>, AppError> {
    Ok(sqlx::query_as::<_, TeamRoleRow>(
        "SELECT tra.role_id, r.name, r.is_system, tra.assigned_at \
           FROM team_role_assignments tra \
           JOIN rbac_roles r ON r.id = tra.role_id \
          WHERE tra.team_id = $1 \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?)
}

/// Assign a role to a team; an existing assignment is left alone.
pub async fn assign_role(
    pool: &PgPool,
    team_id: Uuid,
    role_id: Uuid,
    assigned_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO team_role_assignments (team_id, role_id, assigned_by) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (team_id, role_id) DO NOTHING",
    )
    .bind(team_id)
    .bind(role_id)
    .bind(assigned_by)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_role(pool: &PgPool, team_id: Uuid, role_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM team_role_assignments WHERE team_id = $1 AND role_id = $2")
        .bind(team_id)
        .bind(role_id)
        .execute(pool)
        .await?;
    Ok(())
}
