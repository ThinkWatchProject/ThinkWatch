//! Settings repository — the admin settings handlers' direct reads and
//! writes. Ordinary settings go through `DynamicConfig`; these are the
//! few statements that bypass it.

use sqlx::PgPool;
use think_watch_common::errors::AppError;

/// Drop the OIDC wizard's draft (`oidc.draft`), if any.
pub async fn delete_oidc_draft(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("DELETE FROM system_settings WHERE key = 'oidc.draft'")
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a role with this name exists — what `auth.default_role` may
/// name.
pub async fn role_exists(pool: &PgPool, name: &str) -> Result<bool, AppError> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT name FROM rbac_roles WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(exists.is_some())
}
