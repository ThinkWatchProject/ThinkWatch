#![allow(dead_code)]
// landing zone: some helpers are staged here
// ahead of callers migrating off inline SQL. Remove the allow once
// every function has at least one caller.

//! User repository — thin wrappers over the `users` table so handlers
//! don't carry raw SQL.
//!
//! Extracted out of `handlers::admin` as the first landing zone for the
//! service-layer migration. Every function is a single atomic SQL call
//! that matches an existing handler's `sqlx::query`; no business rules
//! live here (those go into a `UserService` once we need to combine
//! multiple repositories). The repository deliberately takes `&PgPool`
//! OR a `&mut Transaction` per call — mutations that must coexist with
//! a super-admin quorum check belong inside the caller's transaction.

use chrono::{DateTime, Utc};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use uuid::Uuid;

/// Does an active (non-soft-deleted) user with this id exist?
pub async fn exists(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, AppError> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(found)
}

/// Fetch the full user row. Returns `AppError::NotFound` if the id
/// doesn't resolve to an active row — saves every caller a manual
/// `.ok_or(AppError::NotFound(...))`.
pub async fn get_active(pool: &sqlx::PgPool, id: Uuid) -> Result<User, AppError> {
    sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))
}

/// Look up the email for a user id. `None` if the row is
/// soft-deleted or inactive — callers that need the email typically
/// have a valid session already, so the caller decides what to do
/// with a None (usually: 401).
pub async fn find_email(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<String>, AppError> {
    let email = sqlx::query_scalar::<_, String>(
        "SELECT email FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(email)
}

/// Replace the password hash and flag the next login for change.
/// Also bumps `updated_at`, which the temp-password TTL grandfather
/// check reads (see SEC-07 in the login path).
pub async fn update_password_hash(
    pool: &sqlx::PgPool,
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

/// Soft-delete the user. Called by admin::delete_user inside a tx that
/// has already taken the super-admin guard lock and ensured the quorum
/// survives; the repository only touches the row.
pub async fn soft_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    deleted_at: DateTime<Utc>,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE users SET deleted_at = $2, is_active = false, updated_at = now() \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(deleted_at)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() > 0)
}
