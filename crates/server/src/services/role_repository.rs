//! Role repository — the `rbac_roles` catalog and the role side of
//! `rbac_role_assignments`.
//!
//! Thin wrappers over sqlx, one statement per function; policy
//! validation, system-role gating, audit and permission-cache
//! invalidation stay in `handlers::roles`. The statements `delete_role`
//! runs in one transaction take a `&mut PgConnection`.

use sqlx::{PgConnection, PgPool};
use think_watch_common::errors::AppError;
use uuid::Uuid;

/// One row from `rbac_roles` (with creator email LEFT JOINed in):
/// (id, name, description, is_system, policy_document, created_by_email,
/// created_at, updated_at).
pub type RoleRow = (
    Uuid,
    String,
    Option<String>,
    bool,
    serde_json::Value,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

/// One member of a role: (user id, email, display name, scope_kind,
/// scope_id, assigned_at).
pub type RoleMemberRow = (
    Uuid,
    String,
    Option<String>,
    String,
    Option<Uuid>,
    chrono::DateTime<chrono::Utc>,
);

const ROLE_SELECT: &str = "SELECT r.id, r.name, r.description, r.is_system, \
                                  r.policy_document, \
                                  u.email AS created_by_email, \
                                  r.created_at, r.updated_at \
                           FROM rbac_roles r \
                           LEFT JOIN users u ON u.id = r.created_by";

/// Every role's (name, policy_document), for the startup catalog check.
pub async fn policy_documents(
    pool: &PgPool,
) -> Result<Vec<(String, serde_json::Value)>, sqlx::Error> {
    sqlx::query_as("SELECT name, policy_document FROM rbac_roles")
        .fetch_all(pool)
        .await
}

/// Every role, system rows first, then alphabetical.
pub async fn list(pool: &PgPool) -> Result<Vec<RoleRow>, AppError> {
    Ok(
        sqlx::query_as(&format!("{ROLE_SELECT} ORDER BY is_system DESC, name ASC"))
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<RoleRow, AppError> {
    // Qualify with `r.id`: ROLE_SELECT joins `users u`, which also
    // has an `id` column — an unqualified WHERE here used to bubble
    // a 500 from "column reference \"id\" is ambiguous".
    Ok(sqlx::query_as(&format!("{ROLE_SELECT} WHERE r.id = $1"))
        .bind(id)
        .fetch_one(pool)
        .await?)
}

/// A role's (is_system, name).
pub async fn find_kind(pool: &PgPool, id: Uuid) -> Result<Option<(bool, String)>, AppError> {
    Ok(
        sqlx::query_as::<_, (bool, String)>("SELECT is_system, name FROM rbac_roles WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn exists(pool: &PgPool, id: Uuid) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1)")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// [`exists`], inside the caller's transaction.
pub async fn exists_in(conn: &mut PgConnection, id: Uuid) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1)")
            .bind(id)
            .fetch_one(conn)
            .await?,
    )
}

/// The names of whichever of `ids` exist.
pub async fn names_of(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<(String,)>, AppError> {
    Ok(
        sqlx::query_as("SELECT name FROM rbac_roles WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?,
    )
}

/// Create a custom role. The raw error comes back so the caller can
/// report a taken name.
pub async fn insert(
    pool: &PgPool,
    name: &str,
    description: Option<&str>,
    policy_document: &serde_json::Value,
    created_by: Uuid,
) -> Result<RoleRow, sqlx::Error> {
    sqlx::query_as(
        "WITH inserted AS ( \
            INSERT INTO rbac_roles (name, description, is_system, policy_document, created_by) \
            VALUES ($1, $2, FALSE, $3, $4) \
            RETURNING * \
         ) \
         SELECT i.id, i.name, i.description, i.is_system, i.policy_document, \
                u.email AS created_by_email, \
                i.created_at, i.updated_at \
         FROM inserted i \
         LEFT JOIN users u ON u.id = i.created_by",
    )
    .bind(name)
    .bind(description)
    .bind(policy_document)
    .bind(created_by)
    .fetch_one(pool)
    .await
}

/// PATCH a role: `None` name / policy keeps the column; the description
/// is replaced (possibly with NULL) only when `description_set`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
    policy_document: Option<&serde_json::Value>,
    description_set: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE rbac_roles SET \
            name             = COALESCE($2, name), \
            description      = CASE WHEN $5 THEN $3 ELSE description END, \
            policy_document  = COALESCE($4, policy_document), \
            updated_at       = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(policy_document)
    .bind(description_set)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_policy_document(
    pool: &PgPool,
    id: Uuid,
    policy_document: &serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE rbac_roles SET \
            policy_document  = $2, \
            updated_at       = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(policy_document)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a custom role (system rows are never touched).
pub async fn delete_custom(conn: &mut PgConnection, id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM rbac_roles WHERE id = $1 AND is_system = FALSE")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// rbac_role_assignments
// ---------------------------------------------------------------------------

/// (role id, assignment count) for whichever of `ids` have assignments.
pub async fn assignment_counts(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<(Uuid, i64)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT role_id, COUNT(*)::bigint \
           FROM rbac_role_assignments \
          WHERE role_id = ANY($1) \
          GROUP BY role_id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

/// How many assignments a role has. The raw error comes back: callers
/// fall back to 0.
pub async fn assignment_count(pool: &PgPool, id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*)::bigint FROM rbac_role_assignments WHERE role_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// A role's members, by email.
pub async fn members(pool: &PgPool, id: Uuid) -> Result<Vec<RoleMemberRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, ra.scope_kind, ra.scope_id, ra.assigned_at \
           FROM rbac_role_assignments ra \
           JOIN users u ON u.id = ra.user_id \
          WHERE ra.role_id = $1 \
          ORDER BY u.email ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}

/// Copy every (user, scope) assignment of role `from` to role `to`,
/// recording `assigned_by`; pairs `to` already has are skipped.
pub async fn copy_assignments(
    conn: &mut PgConnection,
    from: Uuid,
    to: Uuid,
    assigned_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO rbac_role_assignments \
             (user_id, role_id, scope_kind, scope_id, assigned_by) \
         SELECT user_id, $2, scope_kind, scope_id, $3 \
           FROM rbac_role_assignments WHERE role_id = $1 \
         ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .bind(to)
    .bind(assigned_by)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn delete_assignments(conn: &mut PgConnection, id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM rbac_role_assignments WHERE role_id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}
