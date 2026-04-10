use axum::Json;
use axum::extract::State;
use chrono::Datelike;
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Resolve the caller's analytics scope.
///
/// Returns `Ok(None)` when the caller has `analytics:read_all` at
/// global scope — they see every row in `usage_records`. Otherwise
/// returns `Ok(Some(team_ids))` containing every team the caller is
/// allowed to read; the SQL filter then becomes
/// `WHERE (team_id = ANY($team_ids) OR user_id = $caller)` — caller
/// always sees their own usage even if they're not in any team.
///
/// Falls back to `analytics:read_own` (Some(empty set)) for users
/// who only have own-scoped analytics — they see only their own
/// usage rows.
async fn analytics_team_filter(
    auth_user: &AuthUser,
    pool: &sqlx::PgPool,
) -> Result<Option<Vec<uuid::Uuid>>, AppError> {
    // Global wins outright.
    if auth_user
        .owned_team_scope_for_perm(pool, "analytics:read_all")
        .await?
        .is_none()
    {
        return Ok(None);
    }
    // Otherwise collect team-scoped read_team grants.
    if let Some(set) = auth_user
        .owned_team_scope_for_perm(pool, "analytics:read_team")
        .await?
        && !set.is_empty()
    {
        return Ok(Some(set.into_iter().collect()));
    }
    // No team-level grant either → caller sees only own usage. We
    // express this as an empty team set; the SQL still ORs with
    // user_id = caller so the result is non-empty.
    Ok(Some(Vec::new()))
}

// --- Usage analytics ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UsageStats {
    pub total_tokens_today: i64,
    pub total_requests_today: i64,
}

#[utoipa::path(
    get,
    path = "/api/analytics/usage/stats",
    tag = "Analytics",
    responses(
        (status = 200, description = "Today's usage statistics", body = UsageStats),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_usage_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UsageStats>, AppError> {
    let today = chrono::Utc::now().date_naive();
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    let (total_tokens, total_requests): (Option<i64>, Option<i64>) = match team_filter {
        None => {
            let tokens: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                   FROM usage_records WHERE created_at::date = $1",
            )
            .bind(today)
            .fetch_one(&state.db)
            .await?;
            let reqs: Option<i64> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_records WHERE created_at::date = $1",
            )
            .bind(today)
            .fetch_one(&state.db)
            .await?;
            (tokens, reqs)
        }
        Some(team_ids) => {
            let tokens: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                   FROM usage_records \
                  WHERE created_at::date = $1 \
                    AND (team_id = ANY($2) OR user_id = $3)",
            )
            .bind(today)
            .bind(&team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?;
            let reqs: Option<i64> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_records \
                  WHERE created_at::date = $1 \
                    AND (team_id = ANY($2) OR user_id = $3)",
            )
            .bind(today)
            .bind(&team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?;
            (tokens, reqs)
        }
    };

    Ok(Json(UsageStats {
        total_tokens_today: total_tokens.unwrap_or(0),
        total_requests_today: total_requests.unwrap_or(0),
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct UsageRow {
    pub date: chrono::NaiveDate,
    pub model_id: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[schema(value_type = f64)]
    pub total_cost: rust_decimal::Decimal,
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct AnalyticsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/analytics/usage",
    tag = "Analytics",
    params(AnalyticsQuery),
    responses(
        (status = 200, description = "Usage records grouped by date and model", body = Vec<UsageRow>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_usage(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<AnalyticsQuery>,
) -> Result<Json<Vec<UsageRow>>, AppError> {
    let (limit, offset) =
        super::clickhouse_util::clamp_pagination(params.limit, params.offset, 200);
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    let rows = match team_filter {
        None => {
            sqlx::query_as::<_, UsageRow>(
                r#"SELECT
                    created_at::date as date,
                    model_id,
                    COUNT(*) as request_count,
                    COALESCE(SUM(input_tokens::bigint), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens::bigint), 0)::bigint as output_tokens,
                    COALESCE(SUM(cost_usd), 0) as total_cost
               FROM usage_records
               GROUP BY created_at::date, model_id
               ORDER BY date DESC
               LIMIT $1 OFFSET $2"#,
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(&state.db)
            .await?
        }
        Some(team_ids) => {
            sqlx::query_as::<_, UsageRow>(
                r#"SELECT
                    created_at::date as date,
                    model_id,
                    COUNT(*) as request_count,
                    COALESCE(SUM(input_tokens::bigint), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens::bigint), 0)::bigint as output_tokens,
                    COALESCE(SUM(cost_usd), 0) as total_cost
               FROM usage_records
              WHERE team_id = ANY($3) OR user_id = $4
               GROUP BY created_at::date, model_id
               ORDER BY date DESC
               LIMIT $1 OFFSET $2"#,
            )
            .bind(limit)
            .bind(offset)
            .bind(&team_ids)
            .bind(caller_id)
            .fetch_all(&state.db)
            .await?
        }
    };

    Ok(Json(rows))
}

// --- Cost analytics ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostStats {
    pub total_cost_mtd: f64,
    pub budget_usage_pct: Option<f64>,
}

#[utoipa::path(
    get,
    path = "/api/analytics/costs/stats",
    tag = "Analytics",
    responses(
        (status = 200, description = "Month-to-date cost summary", body = CostStats),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_cost_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<CostStats>, AppError> {
    let month_start = chrono::Utc::now()
        .date_naive()
        .with_day(1)
        .unwrap_or(chrono::Utc::now().date_naive());
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    let total: Option<rust_decimal::Decimal> =
        match team_filter {
            None => sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records WHERE created_at::date >= $1",
            )
            .bind(month_start)
            .fetch_one(&state.db)
            .await?,
            Some(team_ids) => {
                sqlx::query_scalar(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records \
              WHERE created_at::date >= $1 \
                AND (team_id = ANY($2) OR user_id = $3)",
                )
                .bind(month_start)
                .bind(&team_ids)
                .bind(caller_id)
                .fetch_one(&state.db)
                .await?
            }
        };

    use rust_decimal::prelude::ToPrimitive;
    let total_f64 = total.and_then(|d| d.to_f64()).unwrap_or(0.0);

    // `budget_usage_pct` was previously the running MTD total divided
    // by the sum of all team monthly_budget columns. That column is
    // gone (budgets live in `budget_caps` now and are weighted-token
    // denominated, not USD). Phase E rewires this against the new
    // engine; until then the field is always None and the UI hides
    // the bar.
    let budget_usage_pct: Option<f64> = None;

    Ok(Json(CostStats {
        total_cost_mtd: total_f64,
        budget_usage_pct,
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct CostRow {
    pub model_id: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[schema(value_type = f64)]
    pub total_cost: rust_decimal::Decimal,
}

#[utoipa::path(
    get,
    path = "/api/analytics/costs",
    tag = "Analytics",
    params(AnalyticsQuery),
    responses(
        (status = 200, description = "Cost breakdown by model for the current month", body = Vec<CostRow>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_costs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<AnalyticsQuery>,
) -> Result<Json<Vec<CostRow>>, AppError> {
    let (limit, offset) =
        super::clickhouse_util::clamp_pagination(params.limit, params.offset, 200);
    let month_start = chrono::Utc::now()
        .date_naive()
        .with_day(1)
        .unwrap_or(chrono::Utc::now().date_naive());
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    let rows = match team_filter {
        None => {
            sqlx::query_as::<_, CostRow>(
                r#"SELECT
                    model_id,
                    COUNT(*) as request_count,
                    COALESCE(SUM(input_tokens::bigint), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens::bigint), 0)::bigint as output_tokens,
                    COALESCE(SUM(cost_usd), 0) as total_cost
               FROM usage_records
               WHERE created_at::date >= $1
               GROUP BY model_id
               ORDER BY total_cost DESC
               LIMIT $2 OFFSET $3"#,
            )
            .bind(month_start)
            .bind(limit)
            .bind(offset)
            .fetch_all(&state.db)
            .await?
        }
        Some(team_ids) => {
            sqlx::query_as::<_, CostRow>(
                r#"SELECT
                    model_id,
                    COUNT(*) as request_count,
                    COALESCE(SUM(input_tokens::bigint), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens::bigint), 0)::bigint as output_tokens,
                    COALESCE(SUM(cost_usd), 0) as total_cost
               FROM usage_records
               WHERE created_at::date >= $1
                 AND (team_id = ANY($4) OR user_id = $5)
               GROUP BY model_id
               ORDER BY total_cost DESC
               LIMIT $2 OFFSET $3"#,
            )
            .bind(month_start)
            .bind(limit)
            .bind(offset)
            .bind(&team_ids)
            .bind(caller_id)
            .fetch_all(&state.db)
            .await?
        }
    };

    Ok(Json(rows))
}
