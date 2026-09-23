//! `GET /api/dashboard/live` — provider health + RPM buckets + recent
//! log feed for the live panel. Also exposes the shared
//! [`build_live_snapshot`] used by the WebSocket pusher.

use std::pin::Pin;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::{ch_available, ch_client};
use crate::middleware::auth_guard::AuthUser;

use super::scope::resolve_dashboard_user_filter;
use super::top_users::{TopActiveUsersResponse, fetch_top_active_users};

/// Canonical wire shape for `ProviderHealth.kind`. Defined as a real
/// enum so the wire contract is enforced by the type system — adding
/// a third variant requires updating the enum (and the frontend's
/// matching Zod literal union breaks loudly on compile, not at
/// runtime when an unexpected value reaches the dashboard).
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ai,
    Mcp,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProviderHealth {
    pub kind: ProviderKind,
    pub provider: String,
    pub requests: u64,
    pub avg_latency_ms: f64,
    /// Percent of non-throttled requests that did not error. 429
    /// responses are excluded from BOTH numerator and denominator so a
    /// quota-throttled-but-otherwise-healthy upstream reads as
    /// "responsive" instead of "down".
    ///
    /// `None` when there's been no traffic in the window — the
    /// dashboard renders this as "—" instead of a misleading 100%,
    /// which otherwise looks identical to "all calls succeeded."
    pub success_rate: Option<f64>,
    /// Percent of total requests rejected with 429 by the upstream.
    /// Separate signal from `success_rate` — a high `throttled_rate`
    /// means "fix your quota / billing tier," not "upstream is broken."
    /// `None` when there's been no traffic in the window.
    pub throttled_rate: Option<f64>,
    /// Real circuit-breaker state from the gateway runtime.
    /// One of "Closed" / "HalfOpen" / "Open" — both AI providers and MCP
    /// servers write into the same `cb_registry`, so this reflects whichever
    /// gateway last touched the named upstream.
    pub cb_state: String,
}

#[derive(Debug, clickhouse::Row, Deserialize)]
struct ProviderHealthRow {
    provider: String,
    requests: u64,
    avg_latency_ms: f64,
    success_rate: f64,
    throttled_rate: f64,
}

#[derive(Debug, Serialize, clickhouse::Row, Deserialize, utoipa::ToSchema)]
pub struct RpmBucket {
    pub minute: String,
    pub count: u64,
}

/// One row in the unified live feed. Sourced from either `gateway_logs`
/// (AI API requests) or `mcp_logs` (MCP tool calls), normalised so the
/// frontend can render them in a single table.
#[derive(Debug, Serialize, clickhouse::Row, Deserialize, utoipa::ToSchema)]
pub struct LiveLogRow {
    /// "api" for gateway requests, "mcp" for MCP tool calls.
    pub kind: String,
    /// `id` of the MOST RECENT row in the aggregated group — used as
    /// a stable React key, NOT a single-event identifier (the row
    /// represents N events, see `count`).
    pub id: String,
    pub user_id: String,
    /// model_id for "api", tool_name for "mcp".
    pub subject: String,
    /// Status of the most recent event in the group. Numeric HTTP
    /// status for "api" (e.g. "200"), or string status for "mcp"
    /// (e.g. "ok" / "error"). Mixed-status groups surface only
    /// the latest; the count column tells the operator that more
    /// events are folded in.
    pub status: String,
    /// Average latency across the group's events, rounded to ms.
    pub latency_ms: i64,
    /// Sum of tokens across the group's events. "mcp" rows are
    /// summed-zero since the protocol doesn't expose tokens.
    pub tokens: i64,
    /// Timestamp of the latest event in the group.
    pub created_at: String,
    /// How many raw events this row aggregates over the 15-minute
    /// window. `1` means a singleton (no folding); larger values
    /// surface as a `×N` chip next to the subject.
    pub count: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DashboardLive {
    /// Per-provider health stats over the last 15 minutes.
    pub providers: Vec<ProviderHealth>,
    /// Requests per minute for the last 30 minutes (oldest → newest, length 30).
    pub rpm_buckets: Vec<u64>,
    /// Most recent gateway log rows (newest first, up to 14).
    pub recent_logs: Vec<LiveLogRow>,
    /// Highest configured per-key RPM limit across active API keys, if any.
    /// Used as a reference line on the request-rate chart.
    pub max_rpm_limit: Option<i32>,
    /// Top callers over the caller-selected window (24h / 7d / 30d).
    /// Folded into the WS snapshot so the leaderboard refreshes on the
    /// same cadence as the other live tiles — no separate REST poll.
    pub top_users: TopActiveUsersResponse,
}

/// Build a live snapshot. Reused by both the HTTP endpoint and the WS loop.
///
/// `user_filter` is the result of `resolve_dashboard_user_filter` —
/// `None` for global admins, `Some(user_ids)` for team-scoped
/// callers (whose ClickHouse queries gain a `user_id IN (...)`
/// clause).
pub(super) async fn build_live_snapshot(
    state: &AppState,
    user_filter: Option<&[String]>,
    top_users_range: crate::handlers::time_range::TimeRange,
) -> Result<DashboardLive, AppError> {
    // --- Postgres queries: parallel via tokio::try_join! --------------------
    // Errors propagate so the dashboard surfaces a real failure instead of
    // pretending data is empty when the DB is down.
    //
    let providers_fut = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_all(&state.db);
    let mcp_servers_fut =
        sqlx::query_as::<_, (String, String)>("SELECT name, status FROM mcp_servers")
            .fetch_all(&state.db);
    // Highest per-minute RPM limit across all enabled rules — used as
    // a reference line on the request-rate sparkline.
    let rpm_limit_fut = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(max_count) FROM rate_limit_rules \
         WHERE metric = 'requests' AND window_secs = 60 AND enabled = true",
    )
    .fetch_one(&state.db);

    let (configured_providers, configured_mcp_servers, max_rpm_raw) =
        tokio::try_join!(providers_fut, mcp_servers_fut, rpm_limit_fut)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Dashboard PG query failed: {e}")))?;
    let max_rpm_limit: Option<i32> = max_rpm_raw.filter(|&v| v > 0).map(|v| v as i32);

    // Snapshot the in-process CB registry once so we can decorate every
    // provider row with its real state below.
    let cb_states = tw_resil::cb_registry::snapshot_cb_states();

    let seed_provider = |kind: ProviderKind, name: &str| ProviderHealth {
        kind,
        provider: name.to_string(),
        requests: 0,
        avg_latency_ms: 0.0,
        // Zero traffic == no signal. Leaving None lets the frontend
        // render "—" so operators don't read it as "all calls
        // succeeded" — the old 100% default was indistinguishable
        // from a healthy-but-active upstream.
        success_rate: None,
        throttled_rate: None,
        cb_state: cb_states
            .get(name)
            .map(|c| c.as_str().to_string())
            .unwrap_or_else(|| "Closed".to_string()),
    };
    // MCP servers are a special case: when `mcp_servers.status` says
    // "disconnected" we DO want the row to read as down even with zero
    // traffic, because the registry knows the server is unreachable
    // independent of recent log data. For any other status with zero
    // traffic we still surface None so "no recent calls" doesn't
    // masquerade as "all calls succeeded."
    let seed_mcp = |name: &str, status: &str| {
        let cb_state = if status == "disconnected" {
            "Open".to_string()
        } else {
            cb_states
                .get(name)
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "Closed".to_string())
        };
        ProviderHealth {
            kind: ProviderKind::Mcp,
            provider: name.to_string(),
            requests: 0,
            avg_latency_ms: 0.0,
            success_rate: if status == "disconnected" {
                Some(0.0)
            } else {
                None
            },
            // MCP protocol has no rate-limit class; only AI providers
            // populate this.
            throttled_rate: None,
            cb_state,
        }
    };

    if !ch_available(state) {
        let mut providers: Vec<ProviderHealth> = configured_providers
            .iter()
            .map(|(name,)| seed_provider(ProviderKind::Ai, name))
            .collect();
        providers.extend(
            configured_mcp_servers
                .iter()
                .map(|(name, status)| seed_mcp(name, status)),
        );
        return Ok(DashboardLive {
            providers,
            rpm_buckets: vec![0; 30],
            recent_logs: vec![],
            max_rpm_limit,
            top_users: TopActiveUsersResponse {
                users: vec![],
                total: 0,
            },
        });
    }
    let ch = ch_client(state)?;

    // Empty owned set short-circuit. The caller has zero visible
    // users, so any team-filtered query would legitimately return
    // nothing — and ClickHouse rejects an empty IN list at parse
    // time anyway. Skip straight to an empty snapshot.
    if matches!(user_filter, Some([])) {
        return Ok(DashboardLive {
            providers: configured_providers
                .iter()
                .map(|(name,)| seed_provider(ProviderKind::Ai, name))
                .chain(
                    configured_mcp_servers
                        .iter()
                        .map(|(name, status)| seed_mcp(name, status)),
                )
                .collect(),
            rpm_buckets: vec![0; 30],
            recent_logs: vec![],
            max_rpm_limit,
            top_users: TopActiveUsersResponse {
                users: vec![],
                total: 0,
            },
        });
    }

    // --- Four ClickHouse queries in parallel via tokio::try_join! -----------
    //
    // Each query has two SQL variants: an unfiltered one (caller has
    // global analytics:read_all) and a filtered one that constrains
    // `user_id` via the `has(?, user_id)` predicate, where `?` is
    // bound to the caller's visible-user array using the clickhouse
    // crate's parameter binding (NOT string interpolation).
    //
    // The bind path:
    //   - escapes string values via `escape::string` inside the
    //     ClickHouse crate's SQL serializer
    //   - serializes Vec<String> as a CH array literal
    //     `['a','b','c']` (`has` accepts that exactly)
    //   - is the only path the rest of the crate uses for any
    //     untrusted input — see `clickhouse::sql::escape`
    //
    // No format!() / direct string interpolation of `user_id`
    // values anywhere below: SQL injection cannot happen even if
    // `users.id` ever stops being a UUID column.
    //
    // Type wrangling: each match arm's `fetch_all::<T>()` returns a
    // distinct anonymous Future type (different call sites), so the
    // arms won't unify naturally. We Box::pin each future to erase
    // the type — the boxing cost is rounding error compared to a
    // ClickHouse round-trip.
    type ChFut<T> =
        Pin<Box<dyn std::future::Future<Output = clickhouse::error::Result<Vec<T>>> + Send>>;

    let providers_q: ChFut<ProviderHealthRow> = match user_filter {
        None => Box::pin(
            // Read from the 5-minute rollup maintained by
            // provider_health_5m_mv. Scanning pre-aggregated buckets
            // instead of raw gateway_logs drops the per-request row
            // count by roughly (traffic_per_5min × providers), which
            // on a busy deployment is 4-5 orders of magnitude. The
            // user-scoped arm below still hits the raw table because
            // the rollup aggregates user_id out.
            ch.query(
                // success_rate divides by (total - throttled) so 429s
                // don't drag a responsive upstream below 100%.
                // throttled_rate stays on total — operators want to see
                // "what fraction of attempts got rate-limited".
                "SELECT \
                    provider, \
                    toUInt64(sum(total_requests)) AS requests, \
                    if(sum(requests_latency) > 0, \
                       sum(sum_latency_ms) / sum(requests_latency), 0) AS avg_latency_ms, \
                    if(sum(total_requests) - sum(throttled_requests) > 0, \
                       (sum(total_requests) - sum(throttled_requests) - sum(error_requests)) \
                       / (sum(total_requests) - sum(throttled_requests)) * 100, \
                       100) AS success_rate, \
                    if(sum(total_requests) > 0, \
                       sum(throttled_requests) / sum(total_requests) * 100, \
                       0) AS throttled_rate \
                 FROM provider_health_5m \
                 WHERE bucket_5m >= now() - INTERVAL 15 MINUTE \
                 GROUP BY provider \
                 ORDER BY requests DESC \
                 LIMIT 8",
            )
            .fetch_all::<ProviderHealthRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                // Same shape as the global arm, computed directly from
                // gateway_logs because the rollup aggregates user_id out.
                "SELECT \
                    ifNull(provider, 'unknown') AS provider, \
                    count() AS requests, \
                    avg(ifNull(latency_ms, 0)) AS avg_latency_ms, \
                    if(countIf(status_code != 429) > 0, \
                       (countIf(status_code < 400) / countIf(status_code != 429)) * 100, \
                       100) AS success_rate, \
                    if(count() > 0, \
                       (countIf(status_code = 429) / count()) * 100, \
                       0) AS throttled_rate \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                   AND has(?, user_id) \
                 GROUP BY provider \
                 ORDER BY requests DESC \
                 LIMIT 8",
            )
            .bind(ids)
            .fetch_all::<ProviderHealthRow>(),
        ),
    };

    let mcp_q: ChFut<ProviderHealthRow> = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT \
                    ifNull(server_name, 'unknown') AS provider, \
                    count() AS requests, \
                    avg(ifNull(duration_ms, 0)) AS avg_latency_ms, \
                    if(count() > 0, \
                       (countIf(status = 'ok') / count()) * 100, \
                       100) AS success_rate, \
                    toFloat64(0) AS throttled_rate \
                 FROM mcp_logs \
                 PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                 GROUP BY server_name \
                 ORDER BY requests DESC \
                 LIMIT 8",
            )
            .fetch_all::<ProviderHealthRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT \
                    ifNull(server_name, 'unknown') AS provider, \
                    count() AS requests, \
                    avg(ifNull(duration_ms, 0)) AS avg_latency_ms, \
                    if(count() > 0, \
                       (countIf(status = 'ok') / count()) * 100, \
                       100) AS success_rate, \
                    toFloat64(0) AS throttled_rate \
                 FROM mcp_logs \
                 PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                   AND has(?, user_id) \
                 GROUP BY server_name \
                 ORDER BY requests DESC \
                 LIMIT 8",
            )
            .bind(ids)
            .fetch_all::<ProviderHealthRow>(),
        ),
    };

    let buckets_q: ChFut<RpmBucket> = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT \
                    toString(toStartOfMinute(created_at)) AS minute, \
                    count() AS count \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfMinute(now()) - INTERVAL 29 MINUTE \
                 GROUP BY minute \
                 ORDER BY minute ASC",
            )
            .fetch_all::<RpmBucket>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT \
                    toString(toStartOfMinute(created_at)) AS minute, \
                    count() AS count \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfMinute(now()) - INTERVAL 29 MINUTE \
                   AND has(?, user_id) \
                 GROUP BY minute \
                 ORDER BY minute ASC",
            )
            .bind(ids)
            .fetch_all::<RpmBucket>(),
        ),
    };

    // The recent-logs query unions over gateway_logs and mcp_logs.
    // The filtered variant binds the user array TWICE — once per
    // subquery — because the clickhouse crate's `?` placeholders
    // are positional and there's no CTE-style "bind once, use
    // many" facility. The bind takes `impl Serialize` so passing
    // the same `&[String]` slice twice is fine; each call writes
    // its own copy into the SQL during query construction.
    // Aggregate the live feed by (kind, user_id, subject) over the
    // last 15 minutes. Operators reported the raw stream became
    // unreadable when a single user retried the same model 10× in
    // a row — every event got its own line and crowded out activity
    // from other callers. Collapsing to one row per "who×what" tuple
    // with a `count` chip surfaces the same information at a glance.
    //
    // `argMax(status, created_at)` and `argMax(id, created_at)` pull
    // the LATEST event's status + id into the group; sum tokens,
    // avg latency. ORDER BY max(created_at) keeps the most recently
    // active groups on top — matches the original stream's "newest
    // first" ordering.
    // Inner subquery aliases `created_at` → `row_at` so the outer
    // SELECT's `toString(max(...)) AS created_at` alias can't shadow
    // the column inside `argMax(id, created_at)` / `argMax(status,
    // created_at)`. CH resolves alias-vs-column ambiguously when both
    // share a name (the alias wins, which means argMax received an
    // aggregate as its second argument and bailed with ILLEGAL_AGGREGATION).
    let recent_q: ChFut<LiveLogRow> = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT \
                    kind, \
                    cast(argMax(id, row_at) AS String) AS id, \
                    cast(user_id AS String) AS user_id, \
                    cast(subject AS String) AS subject, \
                    cast(argMax(status, row_at) AS String) AS status, \
                    toInt64(round(avg(latency_ms))) AS latency_ms, \
                    toInt64(sum(tokens)) AS tokens, \
                    toString(max(row_at)) AS created_at, \
                    toUInt64(count()) AS count \
                 FROM ( \
                    SELECT \
                        'api' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(model_id, '') AS subject, \
                        toString(ifNull(status_code, 0)) AS status, \
                        ifNull(latency_ms, 0) AS latency_ms, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS tokens, \
                        created_at AS row_at \
                    FROM gateway_logs \
                    PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                    UNION ALL \
                    SELECT \
                        'mcp' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(tool_name, '') AS subject, \
                        ifNull(status, '') AS status, \
                        ifNull(duration_ms, 0) AS latency_ms, \
                        toInt64(0) AS tokens, \
                        created_at AS row_at \
                    FROM mcp_logs \
                    PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                 ) \
                 GROUP BY kind, user_id, subject \
                 ORDER BY max(row_at) DESC \
                 LIMIT 16",
            )
            .fetch_all::<LiveLogRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT \
                    kind, \
                    cast(argMax(id, row_at) AS String) AS id, \
                    cast(user_id AS String) AS user_id, \
                    cast(subject AS String) AS subject, \
                    cast(argMax(status, row_at) AS String) AS status, \
                    toInt64(round(avg(latency_ms))) AS latency_ms, \
                    toInt64(sum(tokens)) AS tokens, \
                    toString(max(row_at)) AS created_at, \
                    toUInt64(count()) AS count \
                 FROM ( \
                    SELECT \
                        'api' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(model_id, '') AS subject, \
                        toString(ifNull(status_code, 0)) AS status, \
                        ifNull(latency_ms, 0) AS latency_ms, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS tokens, \
                        created_at AS row_at \
                    FROM gateway_logs \
                    PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                      AND has(?, user_id) \
                    UNION ALL \
                    SELECT \
                        'mcp' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(tool_name, '') AS subject, \
                        ifNull(status, '') AS status, \
                        ifNull(duration_ms, 0) AS latency_ms, \
                        toInt64(0) AS tokens, \
                        created_at AS row_at \
                    FROM mcp_logs \
                    PREWHERE created_at >= now() - INTERVAL 15 MINUTE \
                      AND has(?, user_id) \
                 ) \
                 GROUP BY kind, user_id, subject \
                 ORDER BY max(row_at) DESC \
                 LIMIT 16",
            )
            .bind(ids)
            .bind(ids)
            .fetch_all::<LiveLogRow>(),
        ),
    };

    let (provider_rows, mcp_rows, buckets_raw, recent_logs) =
        tokio::try_join!(providers_q, mcp_q, buckets_q, recent_q).map_err(|e| {
            AppError::Internal(anyhow::anyhow!("Dashboard ClickHouse query failed: {e}"))
        })?;

    // Merge real CB state into each AI row, then ensure every configured AI
    // provider AND MCP server is represented even with zero traffic.
    // CH returns the SQL fallback (success_rate=100, throttled_rate=0)
    // for a no-traffic bucket — collapse that to None so the wire
    // shape stays honest: "no data" and "perfect score" must not
    // serialize identically.
    let optionalize =
        |requests: u64, rate: f64| -> Option<f64> { if requests == 0 { None } else { Some(rate) } };
    let mut providers: Vec<ProviderHealth> = provider_rows
        .into_iter()
        .map(|r| ProviderHealth {
            kind: ProviderKind::Ai,
            cb_state: cb_states
                .get(&r.provider)
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "Closed".to_string()),
            provider: r.provider,
            success_rate: optionalize(r.requests, r.success_rate),
            throttled_rate: optionalize(r.requests, r.throttled_rate),
            requests: r.requests,
            avg_latency_ms: r.avg_latency_ms,
        })
        .collect();
    for r in mcp_rows {
        providers.push(ProviderHealth {
            kind: ProviderKind::Mcp,
            cb_state: cb_states
                .get(&r.provider)
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "Closed".to_string()),
            provider: r.provider,
            success_rate: optionalize(r.requests, r.success_rate),
            // MCP path doesn't model 429 separately — None on the wire,
            // not 0, so the shape mirrors the AI rows and the frontend
            // doesn't render a misleading "0% throttled" badge.
            throttled_rate: optionalize(r.requests, r.throttled_rate),
            requests: r.requests,
            avg_latency_ms: r.avg_latency_ms,
        });
    }
    for (name,) in &configured_providers {
        if !providers
            .iter()
            .any(|p| matches!(p.kind, ProviderKind::Ai) && &p.provider == name)
        {
            providers.push(seed_provider(ProviderKind::Ai, name));
        }
    }
    for (name, status) in &configured_mcp_servers {
        if !providers
            .iter()
            .any(|p| matches!(p.kind, ProviderKind::Mcp) && &p.provider == name)
        {
            providers.push(seed_mcp(name, status));
        }
    }

    let now = chrono::Utc::now();
    let start_minute = (now - chrono::Duration::minutes(29))
        .format("%Y-%m-%d %H:%M:00")
        .to_string();
    let mut rpm_buckets = vec![0u64; 30];
    for b in buckets_raw {
        if let (Ok(b_dt), Ok(s_dt)) = (
            chrono::NaiveDateTime::parse_from_str(&b.minute, "%Y-%m-%d %H:%M:%S"),
            chrono::NaiveDateTime::parse_from_str(&start_minute, "%Y-%m-%d %H:%M:%S"),
        ) {
            let diff = (b_dt - s_dt).num_minutes();
            if (0..30).contains(&diff) {
                rpm_buckets[diff as usize] = b.count;
            }
        }
    }

    // Top-users runs serially after the snapshot try_join — its query
    // takes ~50-200ms on warm CH, and the snapshot pushes on a 4s tick,
    // so we don't bother folding it into the parallel block (the error
    // types diverge and the saved latency wouldn't be visible to the
    // operator). Caller-selected range is plumbed through from the WS
    // query string.
    let top_users = fetch_top_active_users(state, user_filter, top_users_range).await?;

    Ok(DashboardLive {
        providers,
        rpm_buckets,
        recent_logs,
        max_rpm_limit,
        top_users,
    })
}

#[utoipa::path(
    get,
    path = "/api/dashboard/live",
    tag = "Dashboard",
    responses(
        (status = 200, description = "Live provider health, RPM buckets and recent log entries", body = DashboardLive),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_dashboard_live(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(rq): Query<crate::handlers::time_range::RangeQuery>,
) -> Result<Json<DashboardLive>, AppError> {
    use crate::handlers::time_range::TimeRange;
    let range = TimeRange::parse(rq.range.as_deref());
    let user_filter = resolve_dashboard_user_filter(&state.db, auth_user.claims.sub).await?;
    Ok(Json(
        build_live_snapshot(&state, user_filter.as_deref(), range).await?,
    ))
}
