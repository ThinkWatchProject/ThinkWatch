//! Analytics repository — the Postgres lookups the ClickHouse-backed
//! reports lean on: who is in a team, which cost center an API key
//! carries, a user's email, and an API key's rotation lineage.

use sqlx::PgPool;
use think_watch_common::errors::AppError;
use uuid::Uuid;

/// Distinct members (as text) of any of the given teams.
pub async fn members_of_teams(
    pool: &PgPool,
    team_ids: &[Uuid],
) -> Result<Vec<(String,)>, AppError> {
    Ok(
        sqlx::query_as("SELECT DISTINCT user_id::text FROM team_members WHERE team_id = ANY($1)")
            .bind(team_ids)
            .fetch_all(pool)
            .await?,
    )
}

/// Members (as text) of one team.
pub async fn members_of_team(pool: &PgPool, team_id: Uuid) -> Result<Vec<(String,)>, AppError> {
    Ok(
        sqlx::query_as::<_, (String,)>("SELECT user_id::text FROM team_members WHERE team_id = $1")
            .bind(team_id)
            .fetch_all(pool)
            .await?,
    )
}

/// Each key's cost center, `None` where it has none.
pub async fn cost_centers_of_keys(
    pool: &PgPool,
    key_ids: &[Uuid],
) -> Result<Vec<(Uuid, Option<String>)>, AppError> {
    Ok(
        sqlx::query_as("SELECT id, cost_center FROM api_keys WHERE id = ANY($1)")
            .bind(key_ids)
            .fetch_all(pool)
            .await?,
    )
}

/// Each user's email.
pub async fn emails_of_users(
    pool: &PgPool,
    user_ids: &[Uuid],
) -> Result<Vec<(Uuid, String)>, AppError> {
    Ok(
        sqlx::query_as("SELECT id, email FROM users WHERE id = ANY($1)")
            .bind(user_ids)
            .fetch_all(pool)
            .await?,
    )
}

/// The rotation lineage of an API key, `None` when no such key exists.
/// Returns the raw `sqlx::Error`: callers word the failure themselves.
pub async fn api_key_lineage_id(pool: &PgPool, key_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT lineage_id FROM api_keys WHERE id = $1")
        .bind(key_id)
        .fetch_optional(pool)
        .await
}
