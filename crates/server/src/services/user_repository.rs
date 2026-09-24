//! User repository — the `users` table, a user's rows in
//! `rbac_role_assignments`, and the `api_keys` cascades that follow a
//! user being disabled or deleted.
//!
//! Thin wrappers over sqlx, one statement per function; permission
//! checks, the super-admin quorum guard, audit and cache invalidation
//! stay in `handlers::admin::users`. Statements that must share the
//! caller's transaction (the quorum guard lock, role replacement) take
//! a `&mut PgConnection`.

use sqlx::{PgConnection, PgPool};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use uuid::Uuid;

/// One role assignment of a listed user: (user id, role id, role name,
/// is_system, scope_kind, scope_id).
pub type UserAssignmentRow = (Uuid, Uuid, String, bool, String, Option<Uuid>);

/// One team membership of a listed user: (user id, team id, team name).
pub type UserTeamRow = (Uuid, Uuid, String);

/// One page of live users, newest first, and the total matching
/// `search` (an `ILIKE` pattern on email / display name; `None` = all).
pub async fn list(
    pool: &PgPool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<User>), AppError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users \
          WHERE deleted_at IS NULL \
            AND ($1::text IS NULL OR email ILIKE $1 OR display_name ILIKE $1)",
    )
    .bind(search)
    .fetch_one(pool)
    .await?;
    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users \
          WHERE deleted_at IS NULL \
            AND ($1::text IS NULL OR email ILIKE $1 OR display_name ILIKE $1) \
          ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(search)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok((total, users))
}

/// [`list`], narrowed to `caller` plus every member of `team_ids`.
pub async fn list_in_teams(
    pool: &PgPool,
    caller: Uuid,
    team_ids: &[Uuid],
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<User>), AppError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users u \
          WHERE u.deleted_at IS NULL \
            AND ($3::text IS NULL OR u.email ILIKE $3 OR u.display_name ILIKE $3) \
            AND ( \
                u.id = $1 \
                OR EXISTS ( \
                    SELECT 1 FROM team_members tm \
                     WHERE tm.user_id = u.id \
                       AND tm.team_id = ANY($2) \
                ) \
            )",
    )
    .bind(caller)
    .bind(team_ids)
    .bind(search)
    .fetch_one(pool)
    .await?;
    let users = sqlx::query_as::<_, User>(
        "SELECT u.* FROM users u \
          WHERE u.deleted_at IS NULL \
            AND ($3::text IS NULL OR u.email ILIKE $3 OR u.display_name ILIKE $3) \
            AND ( \
                u.id = $1 \
                OR EXISTS ( \
                    SELECT 1 FROM team_members tm \
                     WHERE tm.user_id = u.id \
                       AND tm.team_id = ANY($2) \
                ) \
            ) \
          ORDER BY u.created_at DESC LIMIT $4 OFFSET $5",
    )
    .bind(caller)
    .bind(team_ids)
    .bind(search)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok((total, users))
}

/// Every role assignment of `user_ids`, system roles first, then by name.
pub async fn role_assignments_of(
    pool: &PgPool,
    user_ids: &[Uuid],
) -> Result<Vec<UserAssignmentRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT ra.user_id, r.id, r.name, r.is_system, ra.scope_kind, ra.scope_id \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = ANY($1) \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(user_ids)
    .fetch_all(pool)
    .await
}

/// Every team membership of `user_ids`, by team name.
pub async fn teams_of(pool: &PgPool, user_ids: &[Uuid]) -> Result<Vec<UserTeamRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT tm.user_id, t.id, t.name \
           FROM team_members tm \
           JOIN teams t ON t.id = tm.team_id \
          WHERE tm.user_id = ANY($1) \
          ORDER BY t.name ASC",
    )
    .bind(user_ids)
    .fetch_all(pool)
    .await
}

/// Is any user row — live or soft-deleted — using this email?
pub async fn email_taken(pool: &PgPool, email: &str) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email = $1)")
            .bind(email)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn insert(
    conn: &mut PgConnection,
    email: &str,
    display_name: &str,
    password_hash: &str,
    password_change_required: bool,
) -> Result<User, AppError> {
    Ok(sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash, password_change_required)
           VALUES ($1, $2, $3, $4) RETURNING *"#,
    )
    .bind(email)
    .bind(display_name)
    .bind(password_hash)
    .bind(password_change_required)
    .fetch_one(conn)
    .await?)
}

/// Does an active (non-soft-deleted) user with this id exist?
pub async fn exists(pool: &PgPool, id: Uuid) -> Result<bool, AppError> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(found)
}

/// Is there an active, live user with this id?
pub async fn active_exists(pool: &PgPool, id: Uuid) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL)",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

pub async fn set_display_name(
    conn: &mut PgConnection,
    id: Uuid,
    display_name: &str,
) -> Result<(), AppError> {
    sqlx::query("UPDATE users SET display_name = $1, updated_at = now() WHERE id = $2")
        .bind(display_name)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn set_active(conn: &mut PgConnection, id: Uuid, active: bool) -> Result<(), AppError> {
    sqlx::query("UPDATE users SET is_active = $1, updated_at = now() WHERE id = $2")
        .bind(active)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Replace the password hash and flag the next login for change.
/// Also bumps `updated_at`, which the temp-password TTL grandfather
/// check reads (see SEC-07 in the login path).
pub async fn update_password_hash(
    pool: &PgPool,
    id: Uuid,
    password_hash: &str,
    force_change: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE users SET password_hash = $1, password_change_required = $2, \
                updated_at = now() WHERE id = $3",
    )
    .bind(password_hash)
    .bind(force_change)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Soft-delete a live user. Returns whether a row was touched.
pub async fn soft_delete(conn: &mut PgConnection, id: Uuid) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE users SET deleted_at = now(), is_active = false, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(conn)
    .await?
    .rows_affected())
}

/// Soft-delete every live API key of a user who was just disabled.
pub async fn disable_api_keys_of_disabled_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE api_keys \
         SET is_active = false, deleted_at = now(), disabled_reason = 'user_disabled' \
         WHERE user_id = $1 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Soft-delete every live API key of a user being deleted.
pub async fn disable_api_keys_of_deleted_user(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE api_keys SET is_active = false, deleted_at = now(), disabled_reason = 'user_deleted' \
         WHERE user_id = $1 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// rbac_role_assignments
// ---------------------------------------------------------------------------

pub async fn delete_role_assignments(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM rbac_role_assignments WHERE user_id = $1")
        .bind(user_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Assign a role (an existing assignment is left alone) and return the
/// role's (name, is_system) — `None` when the role doesn't exist. The raw
/// error comes back so the caller can name an unknown role id.
pub async fn insert_role_assignment(
    conn: &mut PgConnection,
    user_id: Uuid,
    role_id: Uuid,
    scope_kind: &str,
    scope_id: Option<Uuid>,
    assigned_by: Uuid,
) -> Result<Option<(String, bool)>, sqlx::Error> {
    sqlx::query_as(
        "WITH ins AS (\
            INSERT INTO rbac_role_assignments \
                (user_id, role_id, scope_kind, scope_id, assigned_by) \
            VALUES ($1, $2, $3, $4, $5) \
            ON CONFLICT DO NOTHING \
            RETURNING role_id\
         ) \
         SELECT r.name, r.is_system FROM rbac_roles r \
          WHERE r.id = $2",
    )
    .bind(user_id)
    .bind(role_id)
    .bind(scope_kind)
    .bind(scope_id)
    .bind(assigned_by)
    .fetch_optional(conn)
    .await
}
