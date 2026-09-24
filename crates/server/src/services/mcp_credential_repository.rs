//! MCP credential repository — `mcp_user_credentials` (one row per user
//! account on a per-user server) and `mcp_server_shared_credentials`
//! (the single admin-supplied credential of an admin-shared server).
//!
//! Tokens arrive and leave encrypted; encryption, upstream revocation,
//! cache invalidation and audit stay in `handlers::mcp_oauth`.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use think_watch_common::errors::AppError;
use uuid::Uuid;

/// One of the caller's accounts, as listed on `/connections`.
#[derive(sqlx::FromRow)]
pub struct UserCredentialAccountRow {
    pub mcp_server_id: Uuid,
    pub account_label: String,
    pub credential_type: String,
    pub is_default: bool,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub upstream_subject: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What the admin UI shows about a server's shared credential.
#[derive(sqlx::FromRow)]
pub struct SharedCredentialStatusRow {
    pub credential_type: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub upstream_subject: Option<String>,
    pub configured_by: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// mcp_user_credentials
// ---------------------------------------------------------------------------

/// Every account a user holds, grouped by server, default first.
pub async fn list_user_accounts(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<UserCredentialAccountRow>, AppError> {
    Ok(sqlx::query_as::<_, UserCredentialAccountRow>(
        r#"SELECT mcp_server_id, account_label, credential_type, is_default,
                  scopes, expires_at, upstream_subject, created_at, updated_at
             FROM mcp_user_credentials
            WHERE user_id = $1
            ORDER BY mcp_server_id, is_default DESC, account_label"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// An account's credential type and encrypted access token.
pub async fn find_user_token(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
) -> Result<Option<(String, Vec<u8>)>, AppError> {
    Ok(sqlx::query_as(
        r#"SELECT credential_type, access_token_encrypted
             FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .fetch_optional(pool)
    .await?)
}

pub async fn user_account_exists(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
) -> Result<bool, AppError> {
    let exists: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .fetch_optional(pool)
    .await?;
    Ok(exists.is_some())
}

/// Store an account's credential, replacing the one under the same
/// label. The first account a user holds on a server becomes the
/// default.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_user_credential(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
    credential_type: &str,
    access_encrypted: &[u8],
    refresh_encrypted: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scopes: &[String],
    upstream_subject: Option<&str>,
) -> Result<(), AppError> {
    // First credential for (server, user) becomes the default.
    // SELECT-then-INSERT inside one tx is NOT enough on its own —
    // two concurrent first-time inserts (admin opens authorize in two
    // tabs, two account labels) would each read empty + each try
    // is_default=true and the partial unique index
    // `uq_mcp_user_credentials_default` would 23505 the loser into a
    // user-facing 500. Take a per-(server, user) advisory lock so the
    // decision is serialized.
    let mut tx = pool.begin().await?;
    let lock_key = format!("mcp_user_default:{server_id}:{user_id}");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&lock_key)
        .execute(&mut *tx)
        .await?;
    let any_existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 LIMIT 1"#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let new_default = any_existing.is_none();

    sqlx::query(
        r#"INSERT INTO mcp_user_credentials (
               mcp_server_id, user_id, account_label, credential_type, is_default,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (mcp_server_id, user_id, account_label) DO UPDATE SET
               credential_type         = EXCLUDED.credential_type,
               access_token_encrypted  = EXCLUDED.access_token_encrypted,
               refresh_token_encrypted = EXCLUDED.refresh_token_encrypted,
               expires_at              = EXCLUDED.expires_at,
               scopes                  = EXCLUDED.scopes,
               upstream_subject        = EXCLUDED.upstream_subject,
               updated_at              = now()"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .bind(credential_type)
    .bind(new_default)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(scopes)
    .bind(upstream_subject)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Delete an account and, if it was the default, promote the user's
/// newest remaining account on that server, in one transaction.
pub async fn delete_user_credential(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    let was_default: Option<bool> = sqlx::query_scalar(
        r#"DELETE FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3
            RETURNING is_default"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .fetch_optional(&mut *tx)
    .await?;

    if matches!(was_default, Some(true)) {
        // Promote the newest remaining credential for the same
        // (server, user). Newest wins because a user juggling
        // multiple credentials usually treats the latest one as
        // "current" — same heuristic the connect-then-overwrite UX
        // already nudges them toward. NULL `created_at` shouldn't
        // exist (column is NOT NULL DEFAULT now()) but the ORDER BY
        // is still safe under NULLS LAST.
        sqlx::query(
            r#"UPDATE mcp_user_credentials
                SET is_default = true
                WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = (
                    SELECT account_label FROM mcp_user_credentials
                     WHERE mcp_server_id = $1 AND user_id = $2
                     ORDER BY created_at DESC NULLS LAST
                     LIMIT 1
                )"#,
        )
        .bind(server_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Make an account the user's default on its server. Returns `false`
/// (and changes nothing) when the account doesn't exist.
pub async fn set_default_user_credential(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
) -> Result<bool, AppError> {
    let mut tx = pool.begin().await?;
    let exists: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .fetch_optional(&mut *tx)
    .await?;
    if exists.is_none() {
        return Ok(false);
    }

    // Two-step toggle so the partial unique index never sees two
    // is_default rows at once: clear the old default first, then mark
    // the new one inside the same transaction.
    sqlx::query(
        r#"UPDATE mcp_user_credentials SET is_default = false, updated_at = now()
            WHERE mcp_server_id = $1 AND user_id = $2 AND is_default"#,
    )
    .bind(server_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"UPDATE mcp_user_credentials SET is_default = true, updated_at = now()
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// mcp_server_shared_credentials
// ---------------------------------------------------------------------------

/// UPSERT into `mcp_server_shared_credentials`. Single row per server
/// — when the admin rotates the credential the new row replaces the
/// previous one. Uses `INSERT … ON CONFLICT` keyed on the server_id
/// PK so the lifecycle code in `UserTokenResolver` sees a fresh
/// `(access_token_encrypted, expires_at)` after a rotation without
/// any extra coordination.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_shared_credential(
    pool: &PgPool,
    server_id: Uuid,
    credential_type: &str,
    access_encrypted: &[u8],
    refresh_encrypted: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scopes: &[String],
    upstream_subject: Option<&str>,
    configured_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials (
               mcp_server_id, credential_type,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject, configured_by
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (mcp_server_id) DO UPDATE SET
               credential_type         = EXCLUDED.credential_type,
               access_token_encrypted  = EXCLUDED.access_token_encrypted,
               refresh_token_encrypted = EXCLUDED.refresh_token_encrypted,
               expires_at              = EXCLUDED.expires_at,
               scopes                  = EXCLUDED.scopes,
               upstream_subject        = EXCLUDED.upstream_subject,
               configured_by           = EXCLUDED.configured_by,
               updated_at              = now()"#,
    )
    .bind(server_id)
    .bind(credential_type)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(scopes)
    .bind(upstream_subject)
    .bind(configured_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a server's shared credential inside the caller's transaction
/// (the one that inserts the server row).
#[allow(clippy::too_many_arguments)]
pub async fn insert_shared_credential(
    conn: &mut PgConnection,
    server_id: Uuid,
    credential_type: &str,
    access_encrypted: &[u8],
    refresh_encrypted: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scopes: &[String],
    upstream_subject: Option<&str>,
    configured_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials (
               mcp_server_id, credential_type,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject, configured_by
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
    )
    .bind(server_id)
    .bind(credential_type)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(scopes)
    .bind(upstream_subject)
    .bind(configured_by)
    .execute(conn)
    .await?;
    Ok(())
}

/// Insert a pasted static token as a server's shared credential, inside
/// the caller's transaction (the one that inserts the server row).
pub async fn insert_shared_static_token(
    conn: &mut PgConnection,
    server_id: Uuid,
    access_encrypted: &[u8],
    configured_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials (
                   mcp_server_id, credential_type, access_token_encrypted, configured_by
               )
               VALUES ($1, 'static_token', $2, $3)"#,
    )
    .bind(server_id)
    .bind(access_encrypted)
    .bind(configured_by)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn find_shared_status(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Option<SharedCredentialStatusRow>, AppError> {
    Ok(sqlx::query_as::<_, SharedCredentialStatusRow>(
        r#"SELECT credential_type, expires_at, upstream_subject, configured_by, updated_at
             FROM mcp_server_shared_credentials WHERE mcp_server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?)
}

/// The shared credential's type and encrypted access token.
pub async fn find_shared_token(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Option<(String, Vec<u8>)>, AppError> {
    Ok(sqlx::query_as(
        r#"SELECT credential_type, access_token_encrypted
             FROM mcp_server_shared_credentials WHERE mcp_server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn delete_shared_credential(pool: &PgPool, server_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}
