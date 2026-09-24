//! Setup repository — the first-boot wizard's writes: the first super
//! admin, their first API key, and the `setup.*` settings.
//!
//! Everything here runs on the caller's transaction, which holds the
//! setup advisory lock for its whole length so two concurrent setups
//! cannot both pass the "not initialized yet" check.

use sqlx::PgConnection;
use think_watch_common::errors::AppError;
use uuid::Uuid;

/// Take the setup advisory lock (key 1) until the transaction ends.
pub async fn lock_setup(conn: &mut PgConnection) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(1)")
        .execute(conn)
        .await?;
    Ok(())
}

/// The stored `setup.initialized` value, read from the database rather
/// than the settings cache.
pub async fn initialized_flag(
    conn: &mut PgConnection,
) -> Result<Option<serde_json::Value>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT value FROM system_settings WHERE key = 'setup.initialized'")
            .fetch_optional(conn)
            .await?,
    )
}

/// The first super admin and their first key.
pub struct FirstAdmin<'a> {
    pub email: &'a str,
    pub display_name: &'a str,
    pub password_hash: &'a str,
    pub key_prefix: &'a str,
    pub key_hash: &'a str,
    pub key_name: &'a str,
    pub key_surfaces: &'a [&'a str],
    pub site_name: &'a str,
}

/// Create the super admin (global `super_admin` role) and their key,
/// then mark setup done and store the site name. Returns the admin's id
/// and email.
pub async fn create_first_admin(
    conn: &mut PgConnection,
    admin: &FirstAdmin<'_>,
) -> Result<(Uuid, String), AppError> {
    let admin_user = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3) RETURNING id, email"#,
    )
    .bind(admin.email)
    .bind(admin.display_name)
    .bind(admin.password_hash)
    .fetch_one(&mut *conn)
    .await?;
    // A taken email surfaces as `AppError::Conflict` via
    // `From<sqlx::Error>`.

    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = 'super_admin'"#,
    )
    .bind(admin_user.0)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, surfaces)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(admin.key_prefix)
    .bind(admin.key_hash)
    .bind(admin.key_name)
    .bind(admin_user.0)
    .bind(admin.key_surfaces)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE system_settings SET value = $1, updated_at = now() WHERE key = 'setup.initialized'",
    )
    .bind(serde_json::json!(true))
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE system_settings SET value = $1, updated_at = now() WHERE key = 'setup.site_name'",
    )
    .bind(serde_json::json!(admin.site_name))
    .execute(&mut *conn)
    .await?;

    Ok(admin_user)
}
