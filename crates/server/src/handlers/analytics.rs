use axum::Json;
use axum::extract::{Query, State};
use chrono::Datelike;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use think_watch_common::cost_decimal::decode_i128;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::ch_client;
use crate::handlers::time_range::{RangeQuery, TimeRange};
use crate::middleware::auth_guard::AuthUser;

/// Resolve the caller's analytics scope as a user-id allowlist that
/// ClickHouse's `has(?, user_id)` can bind against.
///
/// Returns:
///   - `None` — caller has `analytics:read_all` AND no `team_filter`
///     was requested. Every row in `gateway_logs` is visible.
///   - `Some(user_id_strings)` — the caller is scope-limited (or the
///     caller asked to narrow to a specific team). Set contains the
///     caller's own id, every member of any team they hold
///     `analytics:read_team` for, intersected with `team_filter`'s
///     members when supplied. May be empty (the team filter excluded
///     everything visible).
///
/// `team_filter` is the optional `?team_id=` from the caller; the
/// dropdown on the analytics pages was previously sending it but the
/// handlers ignored it, so the picker did nothing.
///
/// User ids are stringified because `gateway_logs.user_id` is
/// `LowCardinality(Nullable(String))` — the CH query binds an array
/// of strings for the `has()` lookup.
async fn analytics_user_id_filter(
    auth_user: &AuthUser,
    pool: &sqlx::PgPool,
    team_filter: Option<uuid::Uuid>,
) -> Result<Option<Vec<String>>, AppError> {
    // Compute the caller's role-derived scope first.
    let scope: Option<std::collections::HashSet<String>> = if auth_user
        .owned_team_scope_for_perm(pool, "analytics:read_all")
        .await?
        .is_none()
    {
        None
    } else {
        let caller_id = auth_user.claims.sub;
        let mut visible: std::collections::HashSet<String> =
            std::collections::HashSet::from([caller_id.to_string()]);
        if let Some(team_ids) = auth_user
            .owned_team_scope_for_perm(pool, "analytics:read_team")
            .await?
            && !team_ids.is_empty()
        {
            let team_ids_vec: Vec<uuid::Uuid> = team_ids.into_iter().collect();
            let members: Vec<(String,)> = sqlx::query_as(
                "SELECT DISTINCT user_id::text FROM team_members WHERE team_id = ANY($1)",
            )
            .bind(&team_ids_vec)
            .fetch_all(pool)
            .await?;
            for (uid,) in members {
                visible.insert(uid);
            }
        }
        Some(visible)
    };

    // No team filter → return scope as-is.
    let Some(team_id) = team_filter else {
        return Ok(scope.map(|s| s.into_iter().collect()));
    };

    // Team filter requested. Resolve membership and intersect.
    let team_members: std::collections::HashSet<String> =
        sqlx::query_as::<_, (String,)>("SELECT user_id::text FROM team_members WHERE team_id = $1")
            .bind(team_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|(s,)| s)
            .collect();

    let intersected: Vec<String> = match scope {
        None => team_members.into_iter().collect(), // global → just the team
        Some(scope) => scope.intersection(&team_members).cloned().collect(),
    };
    Ok(Some(intersected))
}

/// ClickHouse bucket-start expression for the selected range — mirrors
/// what `TimeRange::trunc_unit` gives PG. Hourly buckets on 24h,
/// daily buckets on 7d/30d.
fn ch_bucket_expr(range: TimeRange) -> &'static str {
    match range {
        TimeRange::Day => "toStartOfHour(created_at)",
        TimeRange::Week | TimeRange::Month => "toStartOfDay(created_at)",
    }
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
    let user_filter = analytics_user_id_filter(&auth_user, &state.db, q.team_id).await?;

    // Empty visible-user set short-circuits. CH rejects an empty IN
    // list at parse time, and the answer is zeros anyway.
    if matches!(user_filter, Some(ref v) if v.is_empty()) {
        return Ok(Json(UsageStats {
            total_tokens: 0,
            total_requests: 0,
            tokens_buckets: vec![0; range.bucket_count()],
            range: range_label(range).into(),
            prev_total_tokens: q.compare.unwrap_or(false).then_some(0),
            prev_total_requests: q.compare.unwrap_or(false).then_some(0),
        }));
    }

    let ch = ch_client(&state)?;
    let window_start_str = window_start.format("%Y-%m-%d %H:%M:%S").to_string();

    // Fold SUM(tokens) + COUNT(*) into one scan. Total tokens =
    // input + output summed across the window.
    #[derive(clickhouse::Row, Deserialize)]
    struct Totals {
        total_tokens: u64,
        total_requests: u64,
    }
    let totals = match &user_filter {
        None => ch
            .query(
                "SELECT \
                    toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS total_tokens, \
                    toUInt64(count()) AS total_requests \
                 FROM gateway_logs \
                 PREWHERE created_at >= parseDateTimeBestEffort(?)",
            )
            .bind(&window_start_str)
            .fetch_one::<Totals>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("usage_stats totals: {e}")))?,
        Some(ids) => ch
            .query(
                "SELECT \
                    toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS total_tokens, \
                    toUInt64(count()) AS total_requests \
                 FROM gateway_logs \
                 PREWHERE created_at >= parseDateTimeBestEffort(?) \
                   AND has(?, user_id)",
            )
            .bind(&window_start_str)
            .bind(ids)
            .fetch_one::<Totals>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("usage_stats totals: {e}")))?,
    };

    // Per-bucket token totals. CH emits `toStartOfHour / toStartOfDay`
    // as the bucket; we align to `range.bucket_starts` in Rust so the
    // zero-fill stays consistent with the PG implementation we replaced.
    #[derive(clickhouse::Row, Deserialize)]
    struct Bucket {
        bucket: String,
        tokens: u64,
    }
    let bucket_expr = ch_bucket_expr(range);
    let bucket_sql_no_filter = format!(
        "SELECT toString({bucket_expr}) AS bucket, \
                toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS tokens \
           FROM gateway_logs \
         PREWHERE created_at >= parseDateTimeBestEffort(?) \
          GROUP BY bucket ORDER BY bucket ASC"
    );
    let bucket_sql_scoped = format!(
        "SELECT toString({bucket_expr}) AS bucket, \
                toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS tokens \
           FROM gateway_logs \
         PREWHERE created_at >= parseDateTimeBestEffort(?) \
            AND has(?, user_id) \
          GROUP BY bucket ORDER BY bucket ASC"
    );
    let bucket_rows: Vec<Bucket> = match &user_filter {
        None => ch
            .query(&bucket_sql_no_filter)
            .bind(&window_start_str)
            .fetch_all::<Bucket>()
            .await
            .unwrap_or_default(),
        Some(ids) => ch
            .query(&bucket_sql_scoped)
            .bind(&window_start_str)
            .bind(ids)
            .fetch_all::<Bucket>()
            .await
            .unwrap_or_default(),
    };
    let lookup: std::collections::HashMap<i64, i64> = bucket_rows
        .into_iter()
        .filter_map(|b| {
            chrono::NaiveDateTime::parse_from_str(&b.bucket, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|ndt| (ndt.and_utc().timestamp(), b.tokens as i64))
        })
        .collect();
    let tokens_buckets: Vec<i64> = range
        .bucket_starts(now)
        .into_iter()
        .map(|t| *lookup.get(&t.timestamp()).unwrap_or(&0))
        .collect();

    // Compare-period totals — same shape, `[prev_start, prev_end)`.
    let (prev_total_tokens, prev_total_requests) = if q.compare.unwrap_or(false) {
        let (prev_start, prev_end) = range.prev_window(now);
        let prev_start_str = prev_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let prev_end_str = prev_end.format("%Y-%m-%d %H:%M:%S").to_string();
        let prev = match &user_filter {
            None => ch
                .query(
                    "SELECT \
                        toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS total_tokens, \
                        toUInt64(count()) AS total_requests \
                     FROM gateway_logs \
                     PREWHERE created_at >= parseDateTimeBestEffort(?) \
                       AND created_at <  parseDateTimeBestEffort(?)",
                )
                .bind(&prev_start_str)
                .bind(&prev_end_str)
                .fetch_one::<Totals>()
                .await
                .ok(),
            Some(ids) => ch
                .query(
                    "SELECT \
                        toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS total_tokens, \
                        toUInt64(count()) AS total_requests \
                     FROM gateway_logs \
                     PREWHERE created_at >= parseDateTimeBestEffort(?) \
                       AND created_at <  parseDateTimeBestEffort(?) \
                       AND has(?, user_id)",
                )
                .bind(&prev_start_str)
                .bind(&prev_end_str)
                .bind(ids)
                .fetch_one::<Totals>()
                .await
                .ok(),
        };
        let prev = prev.unwrap_or(Totals {
            total_tokens: 0,
            total_requests: 0,
        });
        (
            Some(prev.total_tokens as i64),
            Some(prev.total_requests as i64),
        )
    } else {
        (None, None)
    };

    Ok(Json(UsageStats {
        total_tokens: totals.total_tokens as i64,
        total_requests: totals.total_requests as i64,
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UsageRow {
    pub date: chrono::NaiveDate,
    pub model_id: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// USD total for the (date, model) bucket. Serialized as a
    /// string so JS consumers don't downgrade to f64 before they can
    /// feed it into decimal.js — the whole cost pipeline is Decimal
    /// end-to-end now.
    #[schema(value_type = String)]
    #[serde(with = "rust_decimal::serde::str")]
    pub total_cost: Decimal,
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct AnalyticsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Narrow rows to a specific team (only members visible to the
    /// caller are kept; cross-team requests collapse to empty).
    pub team_id: Option<uuid::Uuid>,
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
    let user_filter = analytics_user_id_filter(&auth_user, &state.db, params.team_id).await?;
    if matches!(user_filter, Some(ref v) if v.is_empty()) {
        return Ok(Json(Vec::new()));
    }

    // `total_cost` here is the raw i128 ClickHouse returns for
    // `sum(Decimal(18, 10))` (widened to Decimal(38, 10) under
    // aggregation). `cost_decimal::decode_i128` lifts it back into
    // Decimal at the boundary; the CH crate has no Decimal of its
    // own to deserialize directly into.
    #[derive(clickhouse::Row, Deserialize)]
    struct UsageRowCh {
        date: String,
        model_id: String,
        request_count: u64,
        input_tokens: u64,
        output_tokens: u64,
        total_cost: i128,
    }

    let ch = ch_client(&state)?;
    let ch_rows: Vec<UsageRowCh> = match &user_filter {
        None => ch
            .query(
                "SELECT \
                    toString(toDate(created_at)) AS date, \
                    ifNull(model_id, '') AS model_id, \
                    toUInt64(count()) AS request_count, \
                    toUInt64(sum(ifNull(input_tokens, 0))) AS input_tokens, \
                    toUInt64(sum(ifNull(output_tokens, 0))) AS output_tokens, \
                    sum(ifNull(cost_usd, 0)) AS total_cost \
                 FROM gateway_logs \
                 GROUP BY date, model_id \
                 ORDER BY date DESC \
                 LIMIT ? OFFSET ?",
            )
            .bind(limit)
            .bind(offset)
            .fetch_all::<UsageRowCh>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("usage CH query: {e}")))?,
        Some(ids) => ch
            .query(
                "SELECT \
                    toString(toDate(created_at)) AS date, \
                    ifNull(model_id, '') AS model_id, \
                    toUInt64(count()) AS request_count, \
                    toUInt64(sum(ifNull(input_tokens, 0))) AS input_tokens, \
                    toUInt64(sum(ifNull(output_tokens, 0))) AS output_tokens, \
                    sum(ifNull(cost_usd, 0)) AS total_cost \
                 FROM gateway_logs \
                 PREWHERE has(?, user_id) \
                 GROUP BY date, model_id \
                 ORDER BY date DESC \
                 LIMIT ? OFFSET ?",
            )
            .bind(ids)
            .bind(limit)
            .bind(offset)
            .fetch_all::<UsageRowCh>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("usage CH query: {e}")))?,
    };

    let rows: Vec<UsageRow> = ch_rows
        .into_iter()
        .filter_map(|r| {
            chrono::NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                .ok()
                .map(|date| UsageRow {
                    date,
                    model_id: r.model_id,
                    request_count: r.request_count as i64,
                    input_tokens: r.input_tokens as i64,
                    output_tokens: r.output_tokens as i64,
                    total_cost: decode_i128(r.total_cost),
                })
        })
        .collect();

    Ok(Json(rows))
}

// --- Cost analytics ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostStats {
    /// Total cost (USD) over the selected range, serialized as a
    /// string so the JS side stays precision-safe.
    #[schema(value_type = String)]
    #[serde(with = "rust_decimal::serde::str")]
    pub total_cost: Decimal,
    /// Budget utilisation is a ratio (0..100) computed in Rust from
    /// Redis counters — stays f64, it isn't a money value.
    pub budget_usage_pct: Option<f64>,
    /// Per-bucket cost totals (USD, oldest → newest). Length depends on range.
    pub cost_buckets: Vec<CostBucket>,
    /// Echo of the range the server used.
    pub range: String,
    /// Month-to-date cost, always included regardless of `range`.
    #[schema(value_type = String)]
    #[serde(with = "rust_decimal::serde::str")]
    pub total_cost_mtd: Decimal,
    /// Total cost over the immediately-preceding window of the same
    /// length. Populated only when `?compare=true`.
    #[schema(value_type = Option<String>)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "rust_decimal::serde::str_option")]
    pub prev_total_cost: Option<Decimal>,
}

/// Single bucket's cost — wrapped so we get `rust_decimal::serde::str`
/// on each element without needing a Vec-level helper.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct CostBucket(#[serde(with = "rust_decimal::serde::str")] pub Decimal);

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
    let user_filter = analytics_user_id_filter(&auth_user, &state.db, q.team_id).await?;
    let caller_id = auth_user.claims.sub;

    // Empty visible-user set → all zeros. Short-circuit so the
    // `budget_usage_pct` Redis check still runs below — a scope-zero
    // caller can still have their own budget caps.
    let empty_scope = matches!(user_filter, Some(ref v) if v.is_empty());

    #[derive(clickhouse::Row, Deserialize)]
    struct CostPair {
        total: i128,
        total_mtd: i128,
    }

    let (total_decimal, total_mtd_decimal) = if empty_scope {
        (Decimal::ZERO, Decimal::ZERO)
    } else {
        let ch = ch_client(&state)?;
        let window_start_str = window_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let month_start_str = month_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let row = match &user_filter {
            None => ch
                .query(
                    "SELECT \
                        sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?)) AS total, \
                        sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?)) AS total_mtd \
                     FROM gateway_logs \
                     WHERE created_at >= least(parseDateTimeBestEffort(?), parseDateTimeBestEffort(?))",
                )
                .bind(&window_start_str)
                .bind(&month_start_str)
                .bind(&window_start_str)
                .bind(&month_start_str)
                .fetch_one::<CostPair>()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cost_stats CH query: {e}")))?,
            Some(ids) => ch
                .query(
                    "SELECT \
                        sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?)) AS total, \
                        sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?)) AS total_mtd \
                     FROM gateway_logs \
                     WHERE created_at >= least(parseDateTimeBestEffort(?), parseDateTimeBestEffort(?)) \
                       AND has(?, user_id)",
                )
                .bind(&window_start_str)
                .bind(&month_start_str)
                .bind(&window_start_str)
                .bind(&month_start_str)
                .bind(ids)
                .fetch_one::<CostPair>()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cost_stats CH query: {e}")))?,
        };
        (decode_i128(row.total), decode_i128(row.total_mtd))
    };

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
                        expires_at: None,
                        reason: None,
                        created_by: None,
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

    // Per-bucket cost totals over the selected range. CH row struct
    // keeps the raw i128 (sum widens to Decimal128); we decode to
    // Decimal at the bucket lookup boundary.
    #[derive(clickhouse::Row, Deserialize)]
    struct CostBucketCh {
        bucket: String,
        cost: i128,
    }
    let cost_buckets: Vec<CostBucket> = if empty_scope {
        (0..range.bucket_count())
            .map(|_| CostBucket(Decimal::ZERO))
            .collect()
    } else {
        let ch = ch_client(&state)?;
        let window_start_str = window_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let bucket_expr = ch_bucket_expr(range);
        let sql_no_filter = format!(
            "SELECT toString({bucket_expr}) AS bucket, \
                    sum(ifNull(cost_usd, 0)) AS cost \
               FROM gateway_logs \
             PREWHERE created_at >= parseDateTimeBestEffort(?) \
              GROUP BY bucket ORDER BY bucket ASC"
        );
        let sql_scoped = format!(
            "SELECT toString({bucket_expr}) AS bucket, \
                    sum(ifNull(cost_usd, 0)) AS cost \
               FROM gateway_logs \
             PREWHERE created_at >= parseDateTimeBestEffort(?) \
                AND has(?, user_id) \
              GROUP BY bucket ORDER BY bucket ASC"
        );
        let rows: Vec<CostBucketCh> = match &user_filter {
            None => ch
                .query(&sql_no_filter)
                .bind(&window_start_str)
                .fetch_all::<CostBucketCh>()
                .await
                .unwrap_or_default(),
            Some(ids) => ch
                .query(&sql_scoped)
                .bind(&window_start_str)
                .bind(ids)
                .fetch_all::<CostBucketCh>()
                .await
                .unwrap_or_default(),
        };
        let lookup: std::collections::HashMap<i64, Decimal> = rows
            .into_iter()
            .filter_map(|b| {
                chrono::NaiveDateTime::parse_from_str(&b.bucket, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|ndt| (ndt.and_utc().timestamp(), decode_i128(b.cost)))
            })
            .collect();
        range
            .bucket_starts(now)
            .into_iter()
            .map(|t| CostBucket(lookup.get(&t.timestamp()).copied().unwrap_or(Decimal::ZERO)))
            .collect()
    };

    let prev_total_cost: Option<Decimal> = if q.compare.unwrap_or(false) {
        if empty_scope {
            Some(Decimal::ZERO)
        } else {
            let ch = ch_client(&state)?;
            let (prev_start, prev_end) = range.prev_window(now);
            let prev_start_str = prev_start.format("%Y-%m-%d %H:%M:%S").to_string();
            let prev_end_str = prev_end.format("%Y-%m-%d %H:%M:%S").to_string();
            #[derive(clickhouse::Row, Deserialize)]
            struct PrevCost {
                cost: i128,
            }
            let prev = match &user_filter {
                None => ch
                    .query(
                        "SELECT sum(ifNull(cost_usd, 0)) AS cost FROM gateway_logs \
                         PREWHERE created_at >= parseDateTimeBestEffort(?) \
                            AND created_at <  parseDateTimeBestEffort(?)",
                    )
                    .bind(&prev_start_str)
                    .bind(&prev_end_str)
                    .fetch_one::<PrevCost>()
                    .await
                    .ok(),
                Some(ids) => ch
                    .query(
                        "SELECT sum(ifNull(cost_usd, 0)) AS cost FROM gateway_logs \
                         PREWHERE created_at >= parseDateTimeBestEffort(?) \
                            AND created_at <  parseDateTimeBestEffort(?) \
                            AND has(?, user_id)",
                    )
                    .bind(&prev_start_str)
                    .bind(&prev_end_str)
                    .bind(ids)
                    .fetch_one::<PrevCost>()
                    .await
                    .ok(),
            };
            Some(prev.map(|p| decode_i128(p.cost)).unwrap_or(Decimal::ZERO))
        }
    } else {
        None
    };

    Ok(Json(CostStats {
        total_cost: total_decimal,
        budget_usage_pct,
        cost_buckets,
        range: range_label(range).into(),
        total_cost_mtd: total_mtd_decimal,
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
    #[schema(value_type = String)]
    #[serde(with = "rust_decimal::serde::str")]
    pub total_cost: Decimal,
}

/// Aggregate totals across all groups.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[schema(value_type = String)]
    #[serde(with = "rust_decimal::serde::str")]
    pub total_cost: Decimal,
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

    /// ClickHouse expression that produces this dimension's raw value
    /// from a `gateway_logs` row. For `CostCenter` we carry the
    /// `api_key_id` through CH and remap it to the cost-center label
    /// in Rust — cost_center lives on `api_keys` and isn't snapshotted.
    fn ch_expr(self) -> &'static str {
        match self {
            Self::Model => "ifNull(model_id, '(none)')",
            Self::User => "ifNull(user_id, '(none)')",
            // Raw api_key_id placeholder — the Rust post-pass
            // rewrites this into the cost_center label (or
            // '(untagged)' when no label is set).
            Self::CostCenter => "ifNull(api_key_id, '')",
            // `gateway_logs.provider` is already the snapshotted
            // provider name. We pick that directly instead of
            // joining back to `providers.display_name` because the
            // snapshot survives provider soft-delete.
            Self::Provider => "ifNull(provider, '(none)')",
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
    /// Narrow rows to a specific team (only members visible to the
    /// caller are kept; cross-team requests collapse to empty).
    pub team_id: Option<uuid::Uuid>,
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
    let user_filter = analytics_user_id_filter(&auth_user, &state.db, params.team_id).await?;
    if matches!(user_filter, Some(ref v) if v.is_empty()) {
        return Ok(Json(CostBreakdown {
            items: Vec::new(),
            total: CostTotals {
                request_count: 0,
                input_tokens: 0,
                output_tokens: 0,
                total_cost: Decimal::ZERO,
            },
        })
        .into_response());
    }

    // When CostCenter is in the dim list we fetch more raw rows
    // than the user's LIMIT asks for, because the per-api_key_id
    // grouping may collapse into fewer cost_center rows after
    // enrichment. Cap the over-fetch at 10k — enough for realistic
    // breakdowns, protects against pathological queries.
    let need_cost_center_remap = dims.contains(&CostGroupBy::CostCenter);
    let ch_fetch_limit = if need_cost_center_remap {
        (limit + offset).min(10_000)
    } else {
        limit + offset
    };

    let ch = ch_client(&state)?;
    let window_start_str = window_start.format("%Y-%m-%d %H:%M:%S").to_string();

    // The clickhouse crate needs a concrete Row-deriving struct for
    // every fetch_all, but `dims` has a variable column count
    // (1..=3). Rather than reach for JSONEachRow, we always emit
    // three dimension slots; unused ones evaluate to an empty
    // string literal. GROUP BY always includes all three — grouping
    // by a constant is a no-op — and the Rust pass reads only the
    // dims actually requested. This keeps the Row struct fixed.
    let d1_expr = dims.first().map(|d| d.ch_expr()).unwrap_or("''");
    let d2_expr = dims.get(1).map(|d| d.ch_expr()).unwrap_or("''");
    let d3_expr = dims.get(2).map(|d| d.ch_expr()).unwrap_or("''");

    #[derive(clickhouse::Row, Deserialize)]
    struct CostBreakdownRow {
        dim1: String,
        dim2: String,
        dim3: String,
        request_count: u64,
        input_tokens: u64,
        output_tokens: u64,
        // Raw widened sum from CH (Decimal(38, 10)). Decoded via
        // cost_decimal::decode_i128 once per row below.
        total_cost: i128,
    }

    let sql_no_filter = format!(
        "SELECT \
            {d1_expr} AS dim1, \
            {d2_expr} AS dim2, \
            {d3_expr} AS dim3, \
            toUInt64(count()) AS request_count, \
            toUInt64(sum(ifNull(input_tokens, 0))) AS input_tokens, \
            toUInt64(sum(ifNull(output_tokens, 0))) AS output_tokens, \
            sum(ifNull(cost_usd, 0)) AS total_cost \
         FROM gateway_logs \
         PREWHERE created_at >= parseDateTimeBestEffort(?) \
         GROUP BY dim1, dim2, dim3 \
         ORDER BY total_cost DESC \
         LIMIT ?"
    );
    let sql_scoped = format!(
        "SELECT \
            {d1_expr} AS dim1, \
            {d2_expr} AS dim2, \
            {d3_expr} AS dim3, \
            toUInt64(count()) AS request_count, \
            toUInt64(sum(ifNull(input_tokens, 0))) AS input_tokens, \
            toUInt64(sum(ifNull(output_tokens, 0))) AS output_tokens, \
            sum(ifNull(cost_usd, 0)) AS total_cost \
         FROM gateway_logs \
         PREWHERE created_at >= parseDateTimeBestEffort(?) \
            AND has(?, user_id) \
         GROUP BY dim1, dim2, dim3 \
         ORDER BY total_cost DESC \
         LIMIT ?"
    );
    let ch_rows: Vec<CostBreakdownRow> = match &user_filter {
        None => ch
            .query(&sql_no_filter)
            .bind(&window_start_str)
            .bind(ch_fetch_limit)
            .fetch_all::<CostBreakdownRow>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("get_costs CH: {e}")))?,
        Some(ids) => ch
            .query(&sql_scoped)
            .bind(&window_start_str)
            .bind(ids)
            .bind(ch_fetch_limit)
            .fetch_all::<CostBreakdownRow>()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("get_costs CH: {e}")))?,
    };

    // Stage 1 — rebuild `dim_values` using only the dims the user
    // asked for, picking from the fixed dim1/dim2/dim3 slots.
    struct RawItem {
        dim_values: Vec<String>,
        request_count: i64,
        input_tokens: i64,
        output_tokens: i64,
        total_cost: Decimal,
    }
    let raw_items: Vec<RawItem> = ch_rows
        .into_iter()
        .map(|r| {
            let slot_values = [r.dim1, r.dim2, r.dim3];
            let dim_values: Vec<String> = (0..dims.len()).map(|i| slot_values[i].clone()).collect();
            RawItem {
                dim_values,
                request_count: r.request_count as i64,
                input_tokens: r.input_tokens as i64,
                output_tokens: r.output_tokens as i64,
                total_cost: decode_i128(r.total_cost),
            }
        })
        .collect();

    // Stage 2 — resolve api_key_id → cost_center if needed.
    let key_to_center: std::collections::HashMap<uuid::Uuid, String> = if need_cost_center_remap {
        let cc_idx = dims
            .iter()
            .position(|d| *d == CostGroupBy::CostCenter)
            .expect("cost_center dim present");
        let key_ids: Vec<uuid::Uuid> = raw_items
            .iter()
            .filter_map(|it| uuid::Uuid::parse_str(&it.dim_values[cc_idx]).ok())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if key_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let rows: Vec<(uuid::Uuid, Option<String>)> =
                sqlx::query_as("SELECT id, cost_center FROM api_keys WHERE id = ANY($1)")
                    .bind(&key_ids)
                    .fetch_all(&state.db)
                    .await?;
            rows.into_iter()
                .filter_map(|(id, cc)| cc.map(|c| (id, c)))
                .collect()
        }
    } else {
        std::collections::HashMap::new()
    };

    // Stage 3 — remap raw dim values (cost_center only) and re-aggregate.
    // A single cost_center may absorb multiple api_keys' contributions;
    // totals stay correct because we sum request_count/tokens/cost.
    let cc_idx_opt = dims.iter().position(|d| *d == CostGroupBy::CostCenter);
    type Totals = (i64, i64, i64, Decimal);
    let mut grouped: std::collections::BTreeMap<Vec<String>, Totals> =
        std::collections::BTreeMap::new();
    for it in raw_items {
        let mut dim_values = it.dim_values;
        if let Some(cc_idx) = cc_idx_opt {
            let cc_label = uuid::Uuid::parse_str(&dim_values[cc_idx])
                .ok()
                .and_then(|id| key_to_center.get(&id).cloned())
                .unwrap_or_else(|| "(untagged)".to_string());
            dim_values[cc_idx] = cc_label;
        }
        let entry = grouped
            .entry(dim_values)
            .or_insert((0, 0, 0, Decimal::ZERO));
        entry.0 += it.request_count;
        entry.1 += it.input_tokens;
        entry.2 += it.output_tokens;
        entry.3 += it.total_cost;
    }

    // Sort by total_cost DESC, apply offset+limit.
    let mut ordered: Vec<(Vec<String>, Totals)> = grouped.into_iter().collect();
    ordered.sort_by_key(|b| std::cmp::Reverse(b.1.3));
    let paged: Vec<(Vec<String>, Totals)> = ordered
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    let mut total_req: i64 = 0;
    let mut total_in: i64 = 0;
    let mut total_out: i64 = 0;
    let mut total_cost_sum: Decimal = Decimal::ZERO;
    let items: Vec<CostItem> = paged
        .into_iter()
        .map(|(dim_values, (req, in_tok, out_tok, cost))| {
            total_req += req;
            total_in += in_tok;
            total_out += out_tok;
            total_cost_sum += cost;
            let mut dimensions = std::collections::HashMap::new();
            for (d, v) in dims.iter().zip(dim_values) {
                dimensions.insert(d.key().to_string(), v);
            }
            CostItem {
                dimensions,
                request_count: req,
                input_tokens: in_tok,
                output_tokens: out_tok,
                total_cost: cost,
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
