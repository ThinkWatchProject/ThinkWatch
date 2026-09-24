//! Auth repository — what login, registration, SSO and the account
//! endpoints (`/api/auth/*`) read and write: the caller's `users` row,
//! their role assignments and team memberships, and self-service
//! account deletion.
//!
//! Password hashing, TOTP crypto, lockouts and sessions stay in
//! `handlers::auth` / `handlers::sso` and their services.

use sqlx::{PgConnection, PgExecutor, PgPool};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use uuid::Uuid;

/// A role assignment as `/me` lists it: role id, name, whether it is a
/// system role, scope kind and scope id.
pub type RoleAssignmentRow = (Uuid, String, bool, String, Option<Uuid>);

/// The active, not deleted user with this (normalized) email.
pub async fn find_active_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, AppError> {
    Ok(sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE email = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?)
}

/// The active, not deleted user with this id.
pub async fn find_active(pool: &PgPool, id: Uuid) -> Result<Option<User>, AppError> {
    Ok(sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// The user an SSO identity maps to — deleted or deactivated included,
/// so the caller can refuse those explicitly.
pub async fn find_by_oidc_identity(
    pool: &PgPool,
    subject: &str,
    issuer: &str,
) -> Result<Option<User>, AppError> {
    Ok(sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE oidc_subject = $1 AND oidc_issuer = $2",
    )
    .bind(subject)
    .bind(issuer)
    .fetch_optional(pool)
    .await?)
}

/// `is_active` of a not deleted user.
pub async fn is_active(pool: &PgPool, id: Uuid) -> Result<Option<bool>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT is_active FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// `totp_enabled` of a not deleted user.
pub async fn totp_enabled(pool: &PgPool, id: Uuid) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT totp_enabled FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// Replace the (encrypted) recovery codes if they are still `expected`
/// — a compare-and-swap, so of two requests spending the same code only
/// one wins. Returns how many rows changed (1 = this request won).
pub async fn swap_recovery_codes(
    pool: &PgPool,
    id: Uuid,
    expected: &str,
    updated: &str,
) -> Result<u64, AppError> {
    Ok(sqlx::query(
        "UPDATE users SET totp_recovery_codes = $1 \
                             WHERE id = $2 AND totp_recovery_codes = $3",
    )
    .bind(updated)
    .bind(id)
    .bind(expected)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Insert a self-registered user, or nothing when the email is taken.
pub async fn insert_user_unless_taken(
    conn: &mut PgConnection,
    email: &str,
    display_name: &str,
    password_hash: &str,
) -> Result<Option<User>, AppError> {
    Ok(sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3)
           ON CONFLICT (email) DO NOTHING
           RETURNING *"#,
    )
    .bind(email)
    .bind(display_name)
    .bind(password_hash)
    .fetch_optional(conn)
    .await?)
}

/// Insert a user provisioned by SSO.
pub async fn insert_oidc_user(
    pool: &PgPool,
    email: &str,
    display_name: &str,
    subject: &str,
    issuer: &str,
) -> Result<User, AppError> {
    Ok(sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, oidc_subject, oidc_issuer)
                   VALUES ($1, $2, $3, $4) RETURNING *"#,
    )
    .bind(email)
    .bind(display_name)
    .bind(subject)
    .bind(issuer)
    .fetch_one(pool)
    .await?)
}

/// Give a new user the named role at global scope, self-assigned. A
/// role name that does not exist assigns nothing.
pub async fn assign_default_role<'e>(
    executor: impl PgExecutor<'e>,
    user_id: Uuid,
    role_name: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
                       SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = $2"#,
    )
    .bind(user_id)
    .bind(role_name)
    .execute(executor)
    .await?;
    Ok(())
}

/// The user's teams, by name.
pub async fn teams_of(pool: &PgPool, user_id: Uuid) -> Result<Vec<(Uuid, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.name FROM team_members tm \
           JOIN teams t ON t.id = tm.team_id \
          WHERE tm.user_id = $1 \
          ORDER BY t.name ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// The user's role assignments, system roles first, then by name.
pub async fn role_assignments_of(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<RoleAssignmentRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT r.id, r.name, r.is_system, ra.scope_kind, ra.scope_id \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = $1 \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Set a password the user chose themselves (no forced change next
/// login).
pub async fn set_own_password(
    pool: &PgPool,
    id: Uuid,
    password_hash: &str,
) -> Result<(), AppError> {
    sqlx::query("UPDATE users SET password_hash = $1, password_change_required = false, updated_at = now() WHERE id = $2")
        .bind(password_hash)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Soft-delete the user and every key they own, in one transaction.
pub async fn soft_delete_account(pool: &PgPool, user_id: Uuid) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE api_keys SET is_active = false, deleted_at = now(), disabled_reason = 'account_deleted' WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET is_active = false, deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Turn TOTP on with the (encrypted) secret and recovery codes.
pub async fn enable_totp(
    pool: &PgPool,
    id: Uuid,
    encrypted_secret: &str,
    encrypted_recovery_codes: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE users SET totp_secret = $1, totp_enabled = true, totp_recovery_codes = $2, updated_at = now() WHERE id = $3",
    )
    .bind(encrypted_secret)
    .bind(encrypted_recovery_codes)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Turn TOTP off, dropping the secret and recovery codes.
pub async fn disable_totp(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE users SET totp_secret = NULL, totp_enabled = false, totp_recovery_codes = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
