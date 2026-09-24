//! `GET /api/dashboard/stats` — top-of-page counter tiles for total
//! requests, active providers, active API keys, connected MCP servers.
//! Supports `?range=24h|7d|30d` and `?compare=true` for prev-window
//! deltas.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::{ch_available, ch_client};
use crate::middleware::auth_guard::AuthUser;
use crate::services::observability_repository as repo;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DashboardStats {
    /// Total AI + MCP requests in the selected window.
    pub total_requests: i64,
    pub active_providers: i64,
    /// Number of distinct API keys used in the selected window.
    pub active_api_keys: i64,
    pub connected_mcp_servers: i64,
    /// Per-bucket distinct active key counts (oldest → newest).
    /// Length matches the selected range (24 / 7 / 30).
    pub active_keys_buckets: Vec<u64>,
    /// Echo of the range the server used, so the frontend can label axes.
    pub range: String,
    /// Same totals over the immediately-preceding window of the same
    /// length. Populated only when `?compare=true`. Provider /
    /// MCP-server counts are platform-wide instantaneous values so a
    /// "previous" version doesn't make sense — we only carry the two
    /// windowed counters that do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_total_requests: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_active_api_keys: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/dashboard/stats",
    tag = "Dashboard",
    params(
        ("range" = Option<String>, Query, description = "24h | 7d | 30d (default 24h)"),
    ),
    responses(
        (status = 200, description = "High-level dashboard counters", body = DashboardStats),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_dashboard_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(rq): Query<crate::handlers::time_range::RangeQuery>,
) -> Result<Json<DashboardStats>, AppError> {
    use crate::handlers::time_range::TimeRange;
    let range = TimeRange::parse(rq.range.as_deref());
    let now = chrono::Utc::now();
    let window_start = range.window_start(now);
    let caller_id = auth_user.claims.sub;

    // Determine the team scope for usage / api_key counts. Provider
    // and mcp_server counts are platform-wide and stay global —
    // they're shared resources every team consumes regardless of
    // ownership. Only the per-tenant tiles get filtered.
    let owned_teams_for_keys = auth_user
        .owned_team_scope_for_perm(&state.db, "api_keys:read")
        .await?;
    let owned_teams_for_usage = match auth_user
        .owned_team_scope_for_perm(&state.db, "analytics:read_all")
        .await?
    {
        None => None, // global
        Some(_) => {
            auth_user
                .owned_team_scope_for_perm(&state.db, "analytics:read_team")
                .await?
        }
    };

    // Compute prev-window bounds up-front so the current + compare counts
    // can share a single ClickHouse round-trip (avoids a second scan
    // when `?compare=true`). When compare is off the bounds are unused.
    let want_compare = rq.compare.unwrap_or(false);
    let (prev_start, prev_end) = if want_compare {
        range.prev_window(now)
    } else {
        // Empty window — the countIf clauses below short-circuit to 0.
        (now, now)
    };

    // Expand team scope to a user-id set CH can has(?, user_id) against.
    // Matches the pattern used elsewhere in this file for the active-key
    // counts; keeping the expansion local avoids coupling the two
    // scopes even though they happen to share team-based RBAC.
    let usage_user_filter: Option<Vec<String>> = match &owned_teams_for_usage {
        None => None,
        Some(team_ids) => {
            let team_ids_vec: Vec<uuid::Uuid> = team_ids.iter().copied().collect();
            // Filter out soft-deleted users from the usage scope so a
            // team manager doesn't see analytics rows for accounts that
            // have been removed from the org (those rows linger in CH
            // for the 30-day GDPR retention window).
            let rows =
                repo::caller_and_team_member_ids(&state.db, caller_id, &team_ids_vec).await?;
            Some(rows.into_iter().map(|(s,)| s).collect())
        }
    };

    // Fold "this window" + "previous window" into one CH round-trip via
    // countIf — same shape as the PG FILTER aggregates we used to run.
    // Total requests now live in gateway_logs; the Postgres usage_records
    // table was dropped.
    #[derive(clickhouse::Row, Deserialize)]
    struct ReqCounts {
        current_total: u64,
        prev_total: u64,
    }
    let (total_requests, prev_total_requests_count): (Option<i64>, Option<i64>) = if ch_available(
        &state,
    ) && !matches!(usage_user_filter, Some(ref v) if v.is_empty())
    {
        let ch = ch_client(&state)?;
        let window_start_str = window_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let prev_start_str = prev_start.format("%Y-%m-%d %H:%M:%S").to_string();
        let prev_end_str = prev_end.format("%Y-%m-%d %H:%M:%S").to_string();
        let row = match &usage_user_filter {
                None => ch
                    .query(
                        "SELECT \
                            toUInt64(countIf(created_at >= parseDateTimeBestEffort(?))) AS current_total, \
                            toUInt64(countIf(created_at >= parseDateTimeBestEffort(?) \
                                           AND created_at <  parseDateTimeBestEffort(?))) AS prev_total \
                         FROM gateway_logs \
                         WHERE created_at >= least(parseDateTimeBestEffort(?), parseDateTimeBestEffort(?))",
                    )
                    .bind(&window_start_str)
                    .bind(&prev_start_str)
                    .bind(&prev_end_str)
                    .bind(&window_start_str)
                    .bind(&prev_start_str)
                    .fetch_one::<ReqCounts>()
                    .await
                    .ok(),
                Some(ids) => ch
                    .query(
                        "SELECT \
                            toUInt64(countIf(created_at >= parseDateTimeBestEffort(?))) AS current_total, \
                            toUInt64(countIf(created_at >= parseDateTimeBestEffort(?) \
                                           AND created_at <  parseDateTimeBestEffort(?))) AS prev_total \
                         FROM gateway_logs \
                         WHERE created_at >= least(parseDateTimeBestEffort(?), parseDateTimeBestEffort(?)) \
                           AND has(?, user_id)",
                    )
                    .bind(&window_start_str)
                    .bind(&prev_start_str)
                    .bind(&prev_end_str)
                    .bind(&window_start_str)
                    .bind(&prev_start_str)
                    .bind(ids)
                    .fetch_one::<ReqCounts>()
                    .await
                    .ok(),
            };
        let row = row.unwrap_or(ReqCounts {
            current_total: 0,
            prev_total: 0,
        });
        (Some(row.current_total as i64), Some(row.prev_total as i64))
    } else {
        // Team scope resolved to an empty user set, or CH is
        // disabled — either way the answer is (0, 0).
        (Some(0), Some(0))
    };

    // Filter out soft-deleted rows so the dashboard "active" tile
    // matches what the limits engine and the gateway router actually
    // see. Without `deleted_at IS NULL` the count silently inflates
    // for 30 days after a delete.
    let active_providers = repo::count_active_providers(&state.db).await?;

    let connected_mcp_servers = repo::count_connected_mcp_servers(&state.db).await?;

    // Active API keys — distinct keys used in the selected window from
    // ClickHouse gateway_logs, plus per-bucket counts for the sparkline.
    // Bucket granularity follows the range: hour for 24h, day for 7d/30d.
    let (active_api_keys, active_keys_buckets) = if ch_available(&state) {
        let ch = ch_client(&state)?;

        #[derive(Debug, clickhouse::Row, Deserialize)]
        struct KeyCount {
            cnt: u64,
        }
        #[derive(Debug, clickhouse::Row, Deserialize)]
        struct KeyBucket {
            bucket: String,
            cnt: u64,
        }

        // Scope filter for team-based access — reuse same owned_teams
        // already resolved for usage counts above.
        let visible_user_ids: Option<Vec<String>> = match &owned_teams_for_keys {
            None => None,
            Some(team_ids) => {
                let team_ids_vec: Vec<uuid::Uuid> = team_ids.iter().copied().collect();
                // Same soft-delete filter as the usage scope above —
                // keep the two in lockstep so active-key counts and
                // usage rollups display a consistent population.
                let rows =
                    repo::caller_and_team_member_ids(&state.db, caller_id, &team_ids_vec).await?;
                Some(rows.into_iter().map(|(s,)| s).collect())
            }
        };

        // Map range → ClickHouse interval expression + bucket function.
        // CH doesn't let us parameterise keywords, so we pick one of a fixed
        // set of statements — no string interpolation of untrusted data.
        let count_query = match range {
            TimeRange::Day => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 24 HOUR \
                   AND api_key_id IS NOT NULL AND api_key_id != ''"
            }
            TimeRange::Week => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 7 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != ''"
            }
            TimeRange::Month => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 30 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != ''"
            }
        };
        let count_query_scoped = match range {
            TimeRange::Day => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 24 HOUR \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id)"
            }
            TimeRange::Week => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 7 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id)"
            }
            TimeRange::Month => {
                "SELECT uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= now() - INTERVAL 30 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id)"
            }
        };

        let count_result: u64 = match &visible_user_ids {
            None => ch
                .query(count_query)
                .fetch_one::<KeyCount>()
                .await
                .map(|r| r.cnt)
                .unwrap_or(0),
            Some(ids) => ch
                .query(count_query_scoped)
                .bind(ids)
                .fetch_one::<KeyCount>()
                .await
                .map(|r| r.cnt)
                .unwrap_or(0),
        };

        let bucket_query = match range {
            TimeRange::Day => {
                "SELECT toString(toStartOfHour(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfHour(now()) - INTERVAL 23 HOUR \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
            TimeRange::Week => {
                "SELECT toString(toStartOfDay(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfDay(now()) - INTERVAL 6 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
            TimeRange::Month => {
                "SELECT toString(toStartOfDay(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfDay(now()) - INTERVAL 29 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
        };
        let bucket_query_scoped = match range {
            TimeRange::Day => {
                "SELECT toString(toStartOfHour(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfHour(now()) - INTERVAL 23 HOUR \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id) \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
            TimeRange::Week => {
                "SELECT toString(toStartOfDay(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfDay(now()) - INTERVAL 6 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id) \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
            TimeRange::Month => {
                "SELECT toString(toStartOfDay(created_at)) AS bucket, \
                        uniqExact(api_key_id) AS cnt \
                 FROM gateway_logs \
                 PREWHERE created_at >= toStartOfDay(now()) - INTERVAL 29 DAY \
                   AND api_key_id IS NOT NULL AND api_key_id != '' \
                   AND has(?, user_id) \
                 GROUP BY bucket ORDER BY bucket ASC"
            }
        };

        let bucket_rows: Vec<KeyBucket> = match &visible_user_ids {
            None => ch
                .query(bucket_query)
                .fetch_all::<KeyBucket>()
                .await
                .unwrap_or_default(),
            Some(ids) => ch
                .query(bucket_query_scoped)
                .bind(ids)
                .fetch_all::<KeyBucket>()
                .await
                .unwrap_or_default(),
        };

        // CH emits bucket strings in its default `%Y-%m-%d %H:%M:%S` format.
        // Parse them to timestamps so we can align with range.bucket_starts.
        use chrono::NaiveDateTime;
        let lookup: std::collections::HashMap<i64, u64> = bucket_rows
            .into_iter()
            .filter_map(|b| {
                NaiveDateTime::parse_from_str(&b.bucket, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|ndt| (ndt.and_utc().timestamp(), b.cnt))
            })
            .collect();
        let buckets: Vec<u64> = range
            .bucket_starts(now)
            .into_iter()
            .map(|t| *lookup.get(&t.timestamp()).unwrap_or(&0))
            .collect();
        (count_result as i64, buckets)
    } else {
        // No ClickHouse — fall back to Postgres last_used_at in the window.
        let count = repo::count_api_keys_used_since(&state.db, window_start).await?;
        (count.unwrap_or(0), vec![0; range.bucket_count()])
    };

    // Compare-period totals: same query shape, [prev_start, prev_end).
    // Active key count comes from CH when available, falls back to PG
    // last_used_at — same logic as the current-window branch above.
    // `prev_reqs` was computed alongside `total_requests` in the
    // FILTER-aggregate query at the top of the handler; we only need
    // the CH key-count round-trip here.
    let (prev_total_requests, prev_active_api_keys) = if want_compare {
        let prev_reqs = prev_total_requests_count;

        let prev_keys: i64 = if ch_available(&state) {
            #[derive(Debug, clickhouse::Row, Deserialize)]
            struct KeyCount {
                cnt: u64,
            }
            // Re-bound the CH query against the prev window using the
            // same DateTime literals — avoids mirroring the Day/Week/
            // Month switch above.
            let prev_start_str = prev_start.format("%Y-%m-%d %H:%M:%S").to_string();
            let prev_end_str = prev_end.format("%Y-%m-%d %H:%M:%S").to_string();
            ch_client(&state)?
                .query(
                    "SELECT uniqExact(api_key_id) AS cnt \
                     FROM gateway_logs \
                     WHERE created_at >= parseDateTimeBestEffort(?) \
                       AND created_at <  parseDateTimeBestEffort(?) \
                       AND api_key_id IS NOT NULL AND api_key_id != ''",
                )
                .bind(&prev_start_str)
                .bind(&prev_end_str)
                .fetch_one::<KeyCount>()
                .await
                .map(|r| r.cnt as i64)
                .unwrap_or(0)
        } else {
            repo::count_api_keys_used_between(&state.db, prev_start, prev_end)
                .await?
                .unwrap_or(0)
        };
        (Some(prev_reqs.unwrap_or(0)), Some(prev_keys))
    } else {
        (None, None)
    };

    Ok(Json(DashboardStats {
        total_requests: total_requests.unwrap_or(0),
        active_providers,
        active_api_keys,
        connected_mcp_servers: connected_mcp_servers.unwrap_or(0),
        active_keys_buckets,
        range: match range {
            TimeRange::Day => "24h",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
        }
        .into(),
        prev_total_requests,
        prev_active_api_keys,
    }))
}
