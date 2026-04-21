//! RBAC service — super-admin quorum guard, role-assignment writer,
//! and the `list_super_admin_ids` query.
//!
//! Extracted out of `handlers::admin` so the "don't drop below one
//! active super admin" invariant and the assignment-writer live in
//! one place instead of threading through a 2200-LOC handler file.
//! Handlers call in; any new route that mutates role assignments
//! should go through `write_role_assignments` rather than inlining a
//! DELETE + INSERT pair.

use think_watch_common::errors::AppError;
use uuid::Uuid;

/// Arbitrary 64-bit key for the transaction-scoped advisory lock that
/// serialises every super-admin-role mutation. Must stay stable across
/// releases — two replicas using different keys would defeat the guard.
const SUPER_ADMIN_GUARD_LOCK: i64 = 0x5241_4441_5741_5541; // "RADAWAUA"

/// Take the advisory lock that serialises concurrent super-admin
/// mutations across replicas. Scoped to the calling transaction; the
/// lock releases automatically on COMMIT / ROLLBACK.
pub async fn acquire_super_admin_guard_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SUPER_ADMIN_GUARD_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Guard: after the caller's mutation lands in `tx`, there must still
/// be at least one active (not soft-deleted, not disabled) user who
/// holds the super_admin role — either directly or via team-role
/// assignment. Called after every demote / delete / deactivate / role-
/// strip path. Caller MUST also have taken
/// [`acquire_super_admin_guard_lock`] in the same tx, otherwise a
/// concurrent demote can land between this count and the commit.
pub async fn assert_super_admin_quorum(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), AppError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT u.id) FROM users u \
         JOIN ( \
            SELECT ra.user_id \
              FROM rbac_role_assignments ra \
              JOIN rbac_roles r ON r.id = ra.role_id \
             WHERE r.name = 'super_admin' \
            UNION \
            SELECT tm.user_id \
              FROM team_members tm \
              JOIN team_role_assignments tra ON tra.team_id = tm.team_id \
              JOIN rbac_roles r ON r.id = tra.role_id \
             WHERE r.name = 'super_admin' \
         ) s ON s.user_id = u.id \
         WHERE u.is_active = TRUE AND u.deleted_at IS NULL",
    )
    .fetch_one(&mut **tx)
    .await?;

    if count <= 0 {
        return Err(AppError::BadRequest(
            "Operation would leave the platform with zero active super admins. \
             Add another super admin (directly or via a team role) before continuing."
                .into(),
        ));
    }
    Ok(())
}

/// List of active user ids that currently hold `super_admin` — either
/// directly or transitively via a team role. Read-only sibling of
/// [`assert_super_admin_quorum`], used by the admin UI to grey out the
/// destructive actions on the sole-holder without a round-trip reject.
pub async fn super_admin_ids(pool: &sqlx::PgPool) -> Result<Vec<Uuid>, AppError> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT u.id FROM users u \
         JOIN ( \
            SELECT ra.user_id \
              FROM rbac_role_assignments ra \
              JOIN rbac_roles r ON r.id = ra.role_id \
             WHERE r.name = 'super_admin' \
            UNION \
            SELECT tm.user_id \
              FROM team_members tm \
              JOIN team_role_assignments tra ON tra.team_id = tm.team_id \
              JOIN rbac_roles r ON r.id = tra.role_id \
             WHERE r.name = 'super_admin' \
         ) s ON s.user_id = u.id \
         WHERE u.is_active = TRUE AND u.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}
