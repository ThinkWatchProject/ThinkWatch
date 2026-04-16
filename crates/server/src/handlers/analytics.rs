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

/// A single row in the multi-dimension cost breakdown response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostItem {
    /// Dimension name → display value for each requested `group_by` dimension.
    pub dimensions: std::collections::HashMap<String, String>,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[schema(value_type = f64)]
    pub total_cost: f64,
}

/// Aggregate totals across all groups.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost: f64,
}

/// Top-level cost breakdown response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostBreakdown {
    pub items: Vec<CostItem>,
    pub total: CostTotals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CostGroupBy {
    Model,
    User,
    CostCenter,
    Provider,
}

impl CostGroupBy {
    fn parse_single(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "model" => Some(Self::Model),
            "user" => Some(Self::User),
            "cost_center" | "costcenter" => Some(Self::CostCenter),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }

    /// Parse a comma-separated list of dimensions.
    ///
    /// Returns `Err` with a message for invalid dimension names or if
    /// more than 3 dimensions are requested. An empty / missing value
    /// defaults to `[Model]`.
    fn parse_multi(s: Option<&str>) -> Result<Vec<Self>, String> {
        let raw = s.unwrap_or("").trim();
        if raw.is_empty() {
            return Ok(vec![Self::Model]);
        }
        let mut dims = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match Self::parse_single(part) {
                Some(dim) => {
                    if seen.insert(dim) {
                        dims.push(dim);
                    }
                }
                None => {
                    return Err(format!(
                        "unknown group_by dimension: `{part}`. \
                         Valid values: model, user, cost_center, provider"
                    ));
                }
            }
        }
        if dims.is_empty() {
            return Ok(vec![Self::Model]);
        }
        if dims.len() > 3 {
            return Err("at most 3 group_by dimensions are allowed".into());
        }
        Ok(dims)
    }

    /// The SQL expression that produces this dimension's value.
    fn sql_expr(self) -> &'static str {
        match self {
            Self::Model => "u.model_id",
            Self::User => "COALESCE(u.user_id::text, '(none)')",
            Self::CostCenter => "COALESCE(k.cost_center, '(untagged)')",
            Self::Provider => "COALESCE(p.display_name, '(none)')",
        }
    }

    /// The column alias used in SELECT / GROUP BY for this dimension.
    fn alias(self) -> &'static str {
        match self {
            Self::Model => "dim_model",
            Self::User => "dim_user",
            Self::CostCenter => "dim_cost_center",
            Self::Provider => "dim_provider",
        }
    }

    /// Human-readable dimension key used in the `dimensions` map.
    fn key(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::User => "user",
            Self::CostCenter => "cost_center",
            Self::Provider => "provider",
        }
    }

    /// Whether this dimension requires an extra JOIN.
    fn needs_api_keys_join(self) -> bool {
        matches!(self, Self::CostCenter)
    }

    fn needs_providers_join(self) -> bool {
        matches!(self, Self::Provider)
    }
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct CostsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Comma-separated dimensions: `model` (default), `user`,
    /// `cost_center`, `provider`. Up to 3.
    /// Example: `group_by=model,provider`.
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
        (status = 200, description = "Cost breakdown grouped by the requested dimensions", body = CostBreakdown),
        (status = 400, description = "Invalid group_by dimension"),
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

    let dims =
        CostGroupBy::parse_multi(params.group_by.as_deref()).map_err(AppError::BadRequest)?;

    // Range: default = month-to-date (the page's original semantics).
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
    let team_filter = analytics_team_filter(&auth_user, &state.db).await?;
    let caller_id = auth_user.claims.sub;

    // Build dynamic SQL fragments from the validated dimension list.
    // No user input is interpolated — each enum variant maps to a fixed
    // SQL expression, so there is no injection surface.
    let need_api_keys = dims.iter().any(|d| d.needs_api_keys_join());
    let need_providers = dims.iter().any(|d| d.needs_providers_join());

    let mut from = String::from("usage_records u");
    if need_api_keys {
        from.push_str(" LEFT JOIN api_keys k ON k.id = u.api_key_id");
    }
    if need_providers {
        from.push_str(" LEFT JOIN providers p ON p.id = u.provider_id");
    }

    // SELECT columns: one per dimension alias.
    let select_dims: String = dims
        .iter()
        .map(|d| format!("{} AS {}", d.sql_expr(), d.alias()))
        .collect::<Vec<_>>()
        .join(", ");

    // GROUP BY list — use aliases.
    let group_by_clause: String = dims
        .iter()
        .map(|d| d.alias().to_string())
        .collect::<Vec<_>>()
        .join(", ");

    // Fetch raw rows via sqlx::Row (dynamic column count).
    let base_select = format!(
        "SELECT {select_dims}, \
                COUNT(*) AS request_count, \
                COALESCE(SUM(u.input_tokens::bigint), 0)::bigint AS input_tokens, \
                COALESCE(SUM(u.output_tokens::bigint), 0)::bigint AS output_tokens, \
                COALESCE(SUM(u.cost_usd), 0) AS total_cost \
           FROM {from}"
    );

    use rust_decimal::prelude::ToPrimitive;
    use sqlx::Row;

    let raw_rows: Vec<sqlx::postgres::PgRow> = match &team_filter {
        None => {
            let sql = format!(
                "{base_select} \
                  WHERE u.created_at >= $1 \
                  GROUP BY {group_by_clause} \
                  ORDER BY total_cost DESC \
                  LIMIT $2 OFFSET $3"
            );
            sqlx::query(&sql)
                .bind(window_start)
                .bind(limit)
                .bind(offset)
                .fetch_all(&state.db)
                .await?
        }
        Some(team_ids) => {
            let sql = format!(
                "{base_select} \
                  WHERE u.created_at >= $1 \
                    AND (u.user_id = $5 OR u.user_id IN (\
                         SELECT user_id FROM team_members WHERE team_id = ANY($4))) \
                  GROUP BY {group_by_clause} \
                  ORDER BY total_cost DESC \
                  LIMIT $2 OFFSET $3"
            );
            sqlx::query(&sql)
                .bind(window_start)
                .bind(limit)
                .bind(offset)
                .bind(team_ids)
                .bind(caller_id)
                .fetch_all(&state.db)
                .await?
        }
    };

    // Map raw rows into CostItem structs.
    let mut total_req: i64 = 0;
    let mut total_in: i64 = 0;
    let mut total_out: i64 = 0;
    let mut total_cost_sum: f64 = 0.0;

    let items: Vec<CostItem> = raw_rows
        .iter()
        .map(|row| {
            let mut dimensions = std::collections::HashMap::new();
            for d in &dims {
                let val: String = row.get(d.alias());
                dimensions.insert(d.key().to_string(), val);
            }
            let request_count: i64 = row.get("request_count");
            let input_tokens: i64 = row.get("input_tokens");
            let output_tokens: i64 = row.get("output_tokens");
            let cost_dec: rust_decimal::Decimal = row.get("total_cost");
            let total_cost = cost_dec.to_f64().unwrap_or(0.0);

            total_req += request_count;
            total_in += input_tokens;
            total_out += output_tokens;
            total_cost_sum += total_cost;

            CostItem {
                dimensions,
                request_count,
                input_tokens,
                output_tokens,
                total_cost,
            }
        })
        .collect();

    let breakdown = CostBreakdown {
        items,
        total: CostTotals {
            request_count: total_req,
            input_tokens: total_in,
            output_tokens: total_out,
            total_cost: total_cost_sum,
        },
    };

    // CSV export for spreadsheet / finance handoff.
    if params.format.as_deref() == Some("csv") {
        let dim_headers: String = dims.iter().map(|d| d.key()).collect::<Vec<_>>().join(",");
        let mut body =
            format!("{dim_headers},request_count,input_tokens,output_tokens,total_cost\n");
        for item in &breakdown.items {
            use std::fmt::Write;
            let dim_vals: String = dims
                .iter()
                .map(|d| {
                    csv_escape(
                        item.dimensions
                            .get(d.key())
                            .map(|s| s.as_str())
                            .unwrap_or(""),
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(
                &mut body,
                "{},{},{},{},{}",
                dim_vals,
                item.request_count,
                item.input_tokens,
                item.output_tokens,
                item.total_cost,
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

    Ok(Json(breakdown).into_response())
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
    fn cost_group_by_parses_single_known() {
        let dims = CostGroupBy::parse_multi(None).unwrap();
        assert_eq!(dims, vec![CostGroupBy::Model]);
        assert_eq!(
            CostGroupBy::parse_multi(Some("model")).unwrap(),
            vec![CostGroupBy::Model]
        );
        assert_eq!(
            CostGroupBy::parse_multi(Some("user")).unwrap(),
            vec![CostGroupBy::User]
        );
        assert_eq!(
            CostGroupBy::parse_multi(Some("provider")).unwrap(),
            vec![CostGroupBy::Provider]
        );
        assert_eq!(
            CostGroupBy::parse_multi(Some("cost_center")).unwrap(),
            vec![CostGroupBy::CostCenter]
        );
        assert_eq!(
            CostGroupBy::parse_multi(Some("costcenter")).unwrap(),
            vec![CostGroupBy::CostCenter]
        );
        assert_eq!(
            CostGroupBy::parse_multi(Some("USER")).unwrap(),
            vec![CostGroupBy::User]
        );
    }

    #[test]
    fn cost_group_by_multi_dimensions() {
        let dims = CostGroupBy::parse_multi(Some("model,provider")).unwrap();
        assert_eq!(dims, vec![CostGroupBy::Model, CostGroupBy::Provider]);

        let dims = CostGroupBy::parse_multi(Some("user, cost_center, model")).unwrap();
        assert_eq!(
            dims,
            vec![
                CostGroupBy::User,
                CostGroupBy::CostCenter,
                CostGroupBy::Model
            ]
        );
    }

    #[test]
    fn cost_group_by_deduplicates() {
        let dims = CostGroupBy::parse_multi(Some("model,model,provider")).unwrap();
        assert_eq!(dims, vec![CostGroupBy::Model, CostGroupBy::Provider]);
    }

    #[test]
    fn cost_group_by_rejects_unknown() {
        let err = CostGroupBy::parse_multi(Some("garbage")).unwrap_err();
        assert!(err.contains("unknown group_by dimension"));
    }

    #[test]
    fn cost_group_by_rejects_more_than_3() {
        let err = CostGroupBy::parse_multi(Some("model,user,provider,cost_center")).unwrap_err();
        assert!(err.contains("at most 3"));
    }

    #[test]
    fn cost_group_by_empty_defaults_to_model() {
        assert_eq!(
            CostGroupBy::parse_multi(Some("")).unwrap(),
            vec![CostGroupBy::Model]
        );
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
        assert_eq!(csv_escape("\"quoted\""), "\"\"\"quoted\"\"\"");
    }

    #[test]
    fn csv_escape_keeps_commas_and_newlines_safe() {
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    }
}
