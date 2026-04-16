use axum::Json;
use axum::extract::{Query, State};
use chrono::Datelike;
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::time_range::{RangeQuery, TimeRange};
use crate::middleware::auth_guard::AuthUser;

/// Resolve the caller's analytics scope.
///
/// Returns `Ok(None)` when the caller has `analytics:read_all` at
/// global scope — they see every row in `usage_records`. Otherwise
/// returns `Ok(Some(team_ids))` containing every team the caller is
/// allowed to read; the SQL filter then becomes
/// `WHERE (user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($team_ids))
///     OR user_id = $caller)` — caller always sees their own usage even
/// if they're not in any team. Team scoping flows through the team's
/// members now that `usage_records.team_id` is gone.
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
    /// Total tokens in the selected window.
    pub total_tokens: i64,
    /// Total requests in the selected window.
    pub total_requests: i64,
    /// Per-bucket token totals (oldest → newest). Length = 24 / 7 / 30
    /// depending on the `range` query param.
    pub tokens_buckets: Vec<i64>,
    /// Echo of the range the server used, so the frontend can render labels.
    pub range: String,
    /// Same totals over the immediately-preceding window of the same
    /// length. Populated only when `?compare=true`. Frontend computes
    /// `(current - prev) / prev` to render a percentage delta.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_total_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_total_requests: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/analytics/usage/stats",
    tag = "Analytics",
    params(
        ("range" = Option<String>, Query, description = "24h | 7d | 30d (default 24h)"),
    ),
    responses(
        (status = 200, description = "Usage statistics over the selected range", body = UsageStats),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_usage_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<UsageStats>, AppError> {
    let range = TimeRange::parse(q.range.as_deref());
    let now = chrono::Utc::now();
    let window_start = range.window_start(now);
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    let (total_tokens, total_requests): (Option<i64>, Option<i64>) = match &team_filter {
        None => {
            let tokens: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                   FROM usage_records WHERE created_at >= $1",
            )
            .bind(window_start)
            .fetch_one(&state.db)
            .await?;
            let reqs: Option<i64> =
                sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE created_at >= $1")
                    .bind(window_start)
                    .fetch_one(&state.db)
                    .await?;
            (tokens, reqs)
        }
        Some(team_ids) => {
            let tokens: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                   FROM usage_records \
                  WHERE created_at >= $1 \
                    AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2)))",
            )
            .bind(window_start)
            .bind(team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?;
            let reqs: Option<i64> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_records \
                  WHERE created_at >= $1 \
                    AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2)))",
            )
            .bind(window_start)
            .bind(team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?;
            (tokens, reqs)
        }
    };

    // Per-bucket token totals over the selected range.
    let tokens_buckets = {
        #[derive(sqlx::FromRow)]
        struct Bucket {
            bucket: chrono::DateTime<chrono::Utc>,
            tokens: i64,
        }
        let trunc = range.trunc_unit();
        let sql_common = format!(
            "SELECT date_trunc('{trunc}', created_at) AS bucket, \
                    COALESCE(SUM(total_tokens::bigint), 0)::bigint AS tokens \
               FROM usage_records \
              WHERE created_at >= $1"
        );
        let rows: Vec<Bucket> = match &team_filter {
            None => {
                let sql = format!("{sql_common} GROUP BY bucket ORDER BY bucket");
                sqlx::query_as::<_, Bucket>(&sql)
                    .bind(window_start)
                    .fetch_all(&state.db)
                    .await?
            }
            Some(team_ids) => {
                let sql = format!(
                    "{sql_common} AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2))) \
                     GROUP BY bucket ORDER BY bucket"
                );
                sqlx::query_as::<_, Bucket>(&sql)
                    .bind(window_start)
                    .bind(team_ids)
                    .bind(caller_id)
                    .fetch_all(&state.db)
                    .await?
            }
        };
        let lookup: std::collections::HashMap<i64, i64> = rows
            .into_iter()
            .map(|b| (b.bucket.timestamp(), b.tokens))
            .collect();
        range
            .bucket_starts(now)
            .into_iter()
            .map(|t| *lookup.get(&t.timestamp()).unwrap_or(&0))
            .collect::<Vec<i64>>()
    };

    // Compare-period totals — same query, [prev_start, prev_end) range.
    let (prev_total_tokens, prev_total_requests) = if q.compare.unwrap_or(false) {
        let (prev_start, prev_end) = range.prev_window(now);
        let (pt, pr) = match &team_filter {
            None => {
                let pt: Option<i64> = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                       FROM usage_records WHERE created_at >= $1 AND created_at < $2",
                )
                .bind(prev_start)
                .bind(prev_end)
                .fetch_one(&state.db)
                .await?;
                let pr: Option<i64> = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM usage_records \
                      WHERE created_at >= $1 AND created_at < $2",
                )
                .bind(prev_start)
                .bind(prev_end)
                .fetch_one(&state.db)
                .await?;
                (pt, pr)
            }
            Some(team_ids) => {
                let pt: Option<i64> = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(total_tokens::bigint), 0)::bigint \
                       FROM usage_records \
                      WHERE created_at >= $1 AND created_at < $2 \
                        AND (user_id = $4 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($3)))",
                )
                .bind(prev_start)
                .bind(prev_end)
                .bind(team_ids)
                .bind(caller_id)
                .fetch_one(&state.db)
                .await?;
                let pr: Option<i64> = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM usage_records \
                      WHERE created_at >= $1 AND created_at < $2 \
                        AND (user_id = $4 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($3)))",
                )
                .bind(prev_start)
                .bind(prev_end)
                .bind(team_ids)
                .bind(caller_id)
                .fetch_one(&state.db)
                .await?;
                (pt, pr)
            }
        };
        (Some(pt.unwrap_or(0)), Some(pr.unwrap_or(0)))
    } else {
        (None, None)
    };

    Ok(Json(UsageStats {
        total_tokens: total_tokens.unwrap_or(0),
        total_requests: total_requests.unwrap_or(0),
        tokens_buckets,
        range: range_label(range).into(),
        prev_total_tokens,
        prev_total_requests,
    }))
}

fn range_label(r: TimeRange) -> &'static str {
    match r {
        TimeRange::Day => "24h",
        TimeRange::Week => "7d",
        TimeRange::Month => "30d",
    }
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
              WHERE user_id = $4 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($3))
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
    /// Total cost (USD) over the selected range. For 24h this is "today";
    /// for 7d / 30d it's the last N days. `budget_usage_pct` below remains
    /// tied to the caller's monthly budget caps regardless of range.
    pub total_cost: f64,
    pub budget_usage_pct: Option<f64>,
    /// Per-bucket cost totals (USD, oldest → newest). Length depends on range.
    pub cost_buckets: Vec<f64>,
    /// Echo of the range the server used.
    pub range: String,
    /// Month-to-date cost, always included regardless of `range` — the
    /// "you've spent $X this month" figure needs to stay stable even when
    /// the user is inspecting a 24h window.
    pub total_cost_mtd: f64,
    /// Total cost over the immediately-preceding window of the same
    /// length. Populated only when `?compare=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_total_cost: Option<f64>,
}

#[utoipa::path(
    get,
    path = "/api/analytics/costs/stats",
    tag = "Analytics",
    params(
        ("range" = Option<String>, Query, description = "24h | 7d | 30d (default 24h)"),
    ),
    responses(
        (status = 200, description = "Cost summary over the selected range", body = CostStats),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_cost_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<CostStats>, AppError> {
    let range = TimeRange::parse(q.range.as_deref());
    let now = chrono::Utc::now();
    let window_start = range.window_start(now);
    let month_start = now
        .date_naive()
        .with_day(1)
        .unwrap_or(now.date_naive())
        .and_hms_opt(0, 0, 0)
        .expect("valid hms")
        .and_utc();
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    // Range-scoped total.
    let total: Option<rust_decimal::Decimal> = match &team_filter {
        None => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records WHERE created_at >= $1",
            )
            .bind(window_start)
            .fetch_one(&state.db)
            .await?
        }
        Some(team_ids) => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records \
          WHERE created_at >= $1 \
            AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2)))",
            )
            .bind(window_start)
            .bind(team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?
        }
    };

    // Month-to-date total (independent of range).
    let total_mtd: Option<rust_decimal::Decimal> = match &team_filter {
        None => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records WHERE created_at >= $1",
            )
            .bind(month_start)
            .fetch_one(&state.db)
            .await?
        }
        Some(team_ids) => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records \
          WHERE created_at >= $1 \
            AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2)))",
            )
            .bind(month_start)
            .bind(team_ids)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?
        }
    };

    use rust_decimal::prelude::ToPrimitive;
    let total_f64 = total.and_then(|d| d.to_f64()).unwrap_or(0.0);
    let total_mtd_f64 = total_mtd.and_then(|d| d.to_f64()).unwrap_or(0.0);

    // Budget usage percentage: ratio of current spend to the monthly
    // limit the caller's role-inline AI gateway constraints impose.
    // After the limits refactor, budgets live on `rbac_roles.surface_
    // constraints` (merged most-restrictive across roles) and the
    // Redis counters are keyed per-user.
    let budget_usage_pct: Option<f64> = {
        use think_watch_common::limits::{BudgetCap, BudgetPeriod, BudgetSubject, Surface, budget};
        let constraints =
            think_watch_auth::rbac::compute_user_surface_constraints(&state.db, caller_id)
                .await
                .unwrap_or_default();
        let caps: Vec<BudgetCap> = constraints
            .block(Surface::AiGateway)
            .map(|block| {
                block
                    .budgets
                    .iter()
                    .filter(|b| b.enabled && b.period == BudgetPeriod::Monthly)
                    .map(|b| BudgetCap {
                        id: uuid::Uuid::nil(),
                        subject_kind: BudgetSubject::User,
                        subject_id: caller_id,
                        period: b.period,
                        limit_tokens: b.limit_tokens,
                        enabled: true,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if caps.is_empty() {
            None
        } else {
            let total_limit: i64 = caps.iter().map(|c| c.limit_tokens).sum();
            if total_limit <= 0 {
                None
            } else {
                let statuses = budget::current_spend(&state.redis, &caps)
                    .await
                    .unwrap_or_default();
                let total_current: i64 = statuses.iter().map(|s| s.current).sum();
                Some(total_current as f64 / total_limit as f64 * 100.0)
            }
        }
    };

    // Per-bucket cost totals over the selected range.
    let cost_buckets = {
        #[derive(sqlx::FromRow)]
        struct Bucket {
            bucket: chrono::DateTime<chrono::Utc>,
            cost: rust_decimal::Decimal,
        }
        let trunc = range.trunc_unit();
        let sql_common = format!(
            "SELECT date_trunc('{trunc}', created_at) AS bucket, \
                    COALESCE(SUM(cost_usd), 0) AS cost \
               FROM usage_records \
              WHERE created_at >= $1"
        );
        let rows: Vec<Bucket> = match &team_filter {
            None => {
                let sql = format!("{sql_common} GROUP BY bucket ORDER BY bucket");
                sqlx::query_as::<_, Bucket>(&sql)
                    .bind(window_start)
                    .fetch_all(&state.db)
                    .await?
            }
            Some(team_ids) => {
                let sql = format!(
                    "{sql_common} AND (user_id = $3 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($2))) \
                     GROUP BY bucket ORDER BY bucket"
                );
                sqlx::query_as::<_, Bucket>(&sql)
                    .bind(window_start)
                    .bind(team_ids)
                    .bind(caller_id)
                    .fetch_all(&state.db)
                    .await?
            }
        };
        use rust_decimal::prelude::ToPrimitive;
        let lookup: std::collections::HashMap<i64, f64> = rows
            .into_iter()
            .map(|b| (b.bucket.timestamp(), b.cost.to_f64().unwrap_or(0.0)))
            .collect();
        range
            .bucket_starts(now)
            .into_iter()
            .map(|t| *lookup.get(&t.timestamp()).unwrap_or(&0.0))
            .collect::<Vec<f64>>()
    };

    let prev_total_cost = if q.compare.unwrap_or(false) {
        let (prev_start, prev_end) = range.prev_window(now);
        let prev: Option<rust_decimal::Decimal> = match &team_filter {
            None => {
                sqlx::query_scalar(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records \
                      WHERE created_at >= $1 AND created_at < $2",
                )
                .bind(prev_start)
                .bind(prev_end)
                .fetch_one(&state.db)
                .await?
            }
            Some(team_ids) => {
                sqlx::query_scalar(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_records \
                      WHERE created_at >= $1 AND created_at < $2 \
                        AND (user_id = $4 OR user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($3)))",
                )
                .bind(prev_start)
                .bind(prev_end)
                .bind(team_ids)
                .bind(caller_id)
                .fetch_one(&state.db)
                .await?
            }
        };
        Some(prev.and_then(|d| d.to_f64()).unwrap_or(0.0))
    } else {
        None
    };

    Ok(Json(CostStats {
        total_cost: total_f64,
        budget_usage_pct,
        cost_buckets,
        range: range_label(range).into(),
        total_cost_mtd: total_mtd_f64,
        prev_total_cost,
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct CostRow {
    /// Opaque group key; content depends on `group_by` in the request.
    /// For `group_by=model` it's a model_id; for `user` it's the UUID
    /// as a string; for `cost_center` it's the tag (or the literal
    /// string "(untagged)" for NULL).
    pub group_key: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[schema(value_type = f64)]
    pub total_cost: rust_decimal::Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CostGroupBy {
    Model,
    User,
    CostCenter,
}

impl CostGroupBy {
    fn parse(s: Option<&str>) -> Self {
        // `"team"` was previously a valid value. `usage_records.team_id`
        // has been dropped; a caller asking for the team breakdown now
        // gets the model-level grouping so the page still renders.
        match s.unwrap_or("").to_ascii_lowercase().as_str() {
            "user" => Self::User,
            "cost_center" | "costcenter" => Self::CostCenter,
            _ => Self::Model,
        }
    }
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct CostsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// `model` (default) | `user` | `cost_center`.
    pub group_by: Option<String>,
    /// `json` (default) | `csv` — when `csv`, response is a text/csv
    /// download instead of JSON.
    pub format: Option<String>,
    /// Optional range filter: `24h` | `7d` | `30d` | `mtd` (default).
    /// MTD is what the Costs page has always shown.
    pub range: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/analytics/costs",
    tag = "Analytics",
    params(CostsQuery),
    responses(
        (status = 200, description = "Cost breakdown grouped by the requested dimension", body = Vec<CostRow>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_costs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<CostsQuery>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let (limit, offset) =
        super::clickhouse_util::clamp_pagination(params.limit, params.offset, 200);
    // Range: default = month-to-date (the page's original semantics).
    // Explicit 24h/7d/30d narrow the window.
    let now = chrono::Utc::now();
    let window_start = match params.range.as_deref().unwrap_or("mtd") {
        "24h" => TimeRange::Day.window_start(now),
        "7d" => TimeRange::Week.window_start(now),
        "30d" => TimeRange::Month.window_start(now),
        _ => now
            .date_naive()
            .with_day(1)
            .unwrap_or(now.date_naive())
            .and_hms_opt(0, 0, 0)
            .expect("valid hms")
            .and_utc(),
    };
    let group_by = CostGroupBy::parse(params.group_by.as_deref());
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    // Pick the SQL grouping expression based on group_by. No user input
    // is interpolated — the enum picks one of a fixed set of expressions,
    // so there's no injection surface.
    let (group_expr, from_expr) = match group_by {
        CostGroupBy::Model => ("u.model_id", "usage_records u"),
        CostGroupBy::User => ("COALESCE(u.user_id::text, '(none)')", "usage_records u"),
        CostGroupBy::CostCenter => (
            "COALESCE(k.cost_center, '(untagged)')",
            "usage_records u LEFT JOIN api_keys k ON k.id = u.api_key_id",
        ),
    };

    let rows: Vec<CostRow> = match team_filter {
        None => {
            let sql = format!(
                "SELECT {group_expr} AS group_key, \
                        COUNT(*) AS request_count, \
                        COALESCE(SUM(u.input_tokens::bigint), 0)::bigint AS input_tokens, \
                        COALESCE(SUM(u.output_tokens::bigint), 0)::bigint AS output_tokens, \
                        COALESCE(SUM(u.cost_usd), 0) AS total_cost \
                   FROM {from_expr} \
                  WHERE u.created_at >= $1 \
                  GROUP BY group_key \
                  ORDER BY total_cost DESC \
                  LIMIT $2 OFFSET $3"
            );
            sqlx::query_as::<_, CostRow>(&sql)
                .bind(window_start)
                .bind(limit)
                .bind(offset)
                .fetch_all(&state.db)
                .await?
        }
        Some(team_ids) => {
            let sql = format!(
                "SELECT {group_expr} AS group_key, \
                        COUNT(*) AS request_count, \
                        COALESCE(SUM(u.input_tokens::bigint), 0)::bigint AS input_tokens, \
                        COALESCE(SUM(u.output_tokens::bigint), 0)::bigint AS output_tokens, \
                        COALESCE(SUM(u.cost_usd), 0) AS total_cost \
                   FROM {from_expr} \
                  WHERE u.created_at >= $1 \
                    AND (u.user_id = $5 OR u.user_id IN (SELECT user_id FROM team_members WHERE team_id = ANY($4))) \
                  GROUP BY group_key \
                  ORDER BY total_cost DESC \
                  LIMIT $2 OFFSET $3"
            );
            sqlx::query_as::<_, CostRow>(&sql)
                .bind(window_start)
                .bind(limit)
                .bind(offset)
                .bind(&team_ids)
                .bind(caller_id)
                .fetch_all(&state.db)
                .await?
        }
    };

    // CSV export for spreadsheet / finance handoff.
    if params.format.as_deref() == Some("csv") {
        let mut body =
            String::from("group_key,request_count,input_tokens,output_tokens,total_cost\n");
        for r in &rows {
            use std::fmt::Write;
            let _ = writeln!(
                &mut body,
                "{},{},{},{},{}",
                csv_escape(&r.group_key),
                r.request_count,
                r.input_tokens,
                r.output_tokens,
                r.total_cost,
            );
        }
        let filename = format!("costs-{}.csv", now.format("%Y%m%d"));
        return Ok((
            axum::http::StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    &format!("attachment; filename=\"{filename}\""),
                ),
            ],
            body,
        )
            .into_response());
    }

    Ok(Json(rows).into_response())
}

/// Quote-and-escape a field for RFC4180-ish CSV output. We wrap every
/// field in quotes to avoid ambiguity with commas / newlines inside tags.
fn csv_escape(s: &str) -> String {
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn cost_group_by_parses_known_aliases() {
        assert_eq!(CostGroupBy::parse(None), CostGroupBy::Model);
        assert_eq!(CostGroupBy::parse(Some("model")), CostGroupBy::Model);
        assert_eq!(CostGroupBy::parse(Some("user")), CostGroupBy::User);
        // "team" used to be its own variant; the team column on
        // usage_records is gone, so it now falls through to Model.
        assert_eq!(CostGroupBy::parse(Some("team")), CostGroupBy::Model);
        assert_eq!(
            CostGroupBy::parse(Some("cost_center")),
            CostGroupBy::CostCenter
        );
        assert_eq!(
            CostGroupBy::parse(Some("costcenter")),
            CostGroupBy::CostCenter
        );
        assert_eq!(CostGroupBy::parse(Some("USER")), CostGroupBy::User);
    }

    #[test]
    fn cost_group_by_unknown_falls_back_to_model() {
        // Default to the historical behaviour so a stale client param
        // doesn't 400 the export. Defensive — never trust query strings.
        assert_eq!(CostGroupBy::parse(Some("garbage")), CostGroupBy::Model);
        assert_eq!(CostGroupBy::parse(Some("")), CostGroupBy::Model);
    }

    #[test]
    fn csv_escape_wraps_simple_fields_in_quotes() {
        assert_eq!(csv_escape("hello"), "\"hello\"");
        assert_eq!(csv_escape(""), "\"\"");
    }

    #[test]
    fn csv_escape_doubles_embedded_quotes() {
        // RFC 4180: a literal `"` inside a quoted field becomes `""`.
        assert_eq!(csv_escape(r#"a"b"#), r#""a""b""#);
        // Input is 8 chars: " q u o t e d "
        // Each " becomes "" → ""quoted""
        // Then wrap in "..." → """quoted"""  (3 quotes per side)
        assert_eq!(csv_escape("\"quoted\""), "\"\"\"quoted\"\"\"");
    }

    #[test]
    fn csv_escape_keeps_commas_and_newlines_safe() {
        // The whole field is wrapped in quotes already, so commas and
        // newlines pass through verbatim. Just verify they don't break
        // the wrapping.
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    }
}
