use axum::Json;
use axum::extract::State;
use chrono::Datelike;
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- Usage analytics ---

#[derive(Debug, Serialize)]
pub struct UsageStats {
    pub total_tokens_today: i64,
    pub total_requests_today: i64,
}

pub async fn get_usage_stats(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UsageStats>, AppError> {
    let today = chrono::Utc::now().date_naive();

    let total_tokens: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint FROM usage_records WHERE created_at::date = $1",
    )
    .bind(today)
    .fetch_one(&state.db)
    .await?;

    let total_requests: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE created_at::date = $1")
            .bind(today)
            .fetch_one(&state.db)
            .await?;

    Ok(Json(UsageStats {
        total_tokens_today: total_tokens.unwrap_or(0),
        total_requests_today: total_requests.unwrap_or(0),
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UsageRow {
    pub date: chrono::NaiveDate,
    pub model_id: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost: rust_decimal::Decimal,
}

#[derive(Debug, serde::Deserialize)]
pub struct AnalyticsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn get_usage(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<AnalyticsQuery>,
) -> Result<Json<Vec<UsageRow>>, AppError> {
    let (limit, offset) =
        super::clickhouse_util::clamp_pagination(params.limit, params.offset, 200);

    let rows = sqlx::query_as::<_, UsageRow>(
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
    .await?;

    Ok(Json(rows))
}

// --- Cost analytics ---

#[derive(Debug, Serialize)]
pub struct CostStats {
    pub total_cost_mtd: f64,
    pub budget_usage_pct: Option<f64>,
}

pub async fn get_cost_stats(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<CostStats>, AppError> {
    let month_start = chrono::Utc::now()
        .date_naive()
        .with_day(1)
        .unwrap_or(chrono::Utc::now().date_naive());

    let total: Option<rust_decimal::Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records WHERE created_at::date >= $1",
    )
    .bind(month_start)
    .fetch_one(&state.db)
    .await?;

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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CostRow {
    pub model_id: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost: rust_decimal::Decimal,
}

pub async fn get_costs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<AnalyticsQuery>,
) -> Result<Json<Vec<CostRow>>, AppError> {
    let (limit, offset) =
        super::clickhouse_util::clamp_pagination(params.limit, params.offset, 200);
    let month_start = chrono::Utc::now()
        .date_naive()
        .with_day(1)
        .unwrap_or(chrono::Utc::now().date_naive());

    let rows = sqlx::query_as::<_, CostRow>(
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
    .await?;

    Ok(Json(rows))
}
