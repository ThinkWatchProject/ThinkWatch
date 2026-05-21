//! `GET /api/dashboard/top-users` — leaderboard for the dashboard's
//! bottom-right panel, scoped by the same 24h / 7d / 30d range
//! selector used by the stat cards. Shares the implementation with
//! the live WebSocket snapshot via [`fetch_top_active_users`] — both
//! paths flow through a short-TTL process-local cache so a busy
//! operator dashboard doesn't pin a CH worker on every WS tick.

use std::pin::Pin;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::{ch_available, ch_client};
use crate::middleware::auth_guard::AuthUser;

use super::scope::resolve_dashboard_user_filter;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TopActiveUserRow {
    pub user_id: String,
    /// Best-effort display label — `user_email` snapshotted at write
    /// time on each `gateway_logs` / `mcp_logs` row, falling back to
    /// empty when the row predates the email column or the caller was
    /// unauthenticated.
    pub user_email: String,
    /// AI gateway requests from this user in the window
    /// (`gateway_logs.count()`).
    pub request_count: u64,
    /// Sum of input+output tokens across this user's AI requests in
    /// the window. MCP calls don't carry token counts.
    pub total_tokens: i64,
    /// MCP tool calls from this user in the window
    /// (`mcp_logs.count()`). Distinct from `request_count` so
    /// dashboards can size each lane independently — a user who
    /// only triggers MCP traffic shouldn't read as "inactive."
    pub mcp_call_count: u64,
    /// ISO-8601 UTC timestamp of the most recent gateway OR mcp
    /// activity from this user in the window.
    pub last_active: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TopActiveUsersResponse {
    pub users: Vec<TopActiveUserRow>,
    /// Distinct active users in the window (NOT capped to `users.len()`).
    /// Surfaced on the panel header so operators can tell "50 / 50"
    /// (cap hit, full data not shown) from "8 / 8" (all visible).
    pub total: u64,
}

// Hard cap on the result set. The panel is scrollable, but we don't
// want to ship a 10k-row payload for a 30-day window — operators
// realistically care about the top tens, and beyond ~100 the panel
// loses signal.
const TOP_USERS_LIMIT: u32 = 50;

#[derive(Debug, clickhouse::Row, Deserialize)]
struct TopActiveUserChRow {
    user_id: String,
    user_email: String,
    request_count: u64,
    total_tokens: i64,
    mcp_call_count: u64,
    last_active: String,
}

#[utoipa::path(
    get,
    path = "/api/dashboard/top-users",
    tag = "Dashboard",
    params(
        ("range" = Option<String>, Query, description = "24h | 7d | 30d (default 24h)"),
    ),
    responses(
        (status = 200, description = "Top callers ranked by request count over the window", body = TopActiveUsersResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_top_active_users(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(rq): Query<crate::handlers::time_range::RangeQuery>,
) -> Result<Json<TopActiveUsersResponse>, AppError> {
    use crate::handlers::time_range::TimeRange;
    let range = TimeRange::parse(rq.range.as_deref());
    let user_filter = resolve_dashboard_user_filter(&state.db, auth_user.claims.sub).await?;
    Ok(Json(
        fetch_top_active_users(&state, user_filter.as_deref(), range).await?,
    ))
}

/// Short-TTL process-local cache fronting the top-users CH query.
///
/// The live snapshot pushes every 4 s per connected dashboard user;
/// without this, every tick hit ClickHouse with two scans across
/// `gateway_logs ∪ mcp_logs` over the full 24h / 7d / 30d window.
/// 15 s is short enough that a new caller appearing in the top 50
/// becomes visible within 3-4 WS ticks, and long enough that a busy
/// operator dashboard collapses to one CH round-trip per minute
/// instead of 15.
const TOP_USERS_TTL: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Hash, Eq, PartialEq, Clone)]
struct TopUsersCacheKey {
    /// Fingerprint of the caller's RBAC scope. Distinct sets ⇒
    /// distinct results, so they must not share cache slots.
    filter_hash: u64,
    range: crate::handlers::time_range::TimeRange,
}

type TopUsersCache = std::sync::Mutex<
    std::collections::HashMap<TopUsersCacheKey, (std::time::Instant, TopActiveUsersResponse)>,
>;

fn top_users_cache() -> &'static TopUsersCache {
    static CACHE: std::sync::OnceLock<TopUsersCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn user_filter_hash(filter: Option<&[String]>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match filter {
        // Tag the variant so `None` and `Some(&[])` hash to different
        // slots — the empty-scope short-circuit yields an empty
        // result while `None` (global read_all) yields the platform-
        // wide leaderboard; they must not share cache lines.
        None => 0u8.hash(&mut h),
        Some(ids) => {
            1u8.hash(&mut h);
            // Sort before hashing — PG doesn't guarantee row order
            // without an explicit ORDER BY (the query in
            // `resolve_dashboard_user_filter` has none), so a plan
            // change could reorder the same membership and produce
            // a fresh cache miss every 32 s revoke tick. Sorting a
            // tiny Vec (at most a few hundred UUIDs) keeps the hash
            // deterministic for the same set regardless of order.
            let mut sorted: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            sorted.sort_unstable();
            for id in sorted {
                id.hash(&mut h);
            }
        }
    }
    h.finish()
}

/// Shared implementation behind both `/api/dashboard/top-users` (REST)
/// and the live WebSocket snapshot. Cached for 15 s per (filter, range)
/// so a busy operator dashboard doesn't pin a CH worker on every WS
/// tick. Folds the empty-CH / empty-scope short-circuits in one place
/// so the two call sites can't drift.
pub(super) async fn fetch_top_active_users(
    state: &AppState,
    user_filter: Option<&[String]>,
    range: crate::handlers::time_range::TimeRange,
) -> Result<TopActiveUsersResponse, AppError> {
    let key = TopUsersCacheKey {
        filter_hash: user_filter_hash(user_filter),
        range,
    };

    // Fast path: serve from cache if the slot is still fresh. The
    // lock is released before any await — no holding across yields.
    // A poisoned mutex falls through to the uncached path *and*
    // logs once so the silent degradation has a signal.
    match top_users_cache().lock() {
        Ok(guard) => {
            if let Some((stored_at, cached)) = guard.get(&key)
                && stored_at.elapsed() < TOP_USERS_TTL
            {
                return Ok(cached.clone());
            }
        }
        Err(e) => tracing::warn!("top_users_cache mutex poisoned on read; bypassing cache: {e}"),
    }

    let resp = fetch_top_active_users_uncached(state, user_filter, range).await?;

    match top_users_cache().lock() {
        Ok(mut guard) => {
            // Opportunistic prune on insert: the cache key is
            // (filter_hash, range), and filter_hash varies per RBAC
            // scope. Without eviction the map grows with every
            // distinct scope ever observed. Sweep entries older than
            // 2×TTL — anything fresher MIGHT still be a fast-path
            // hit, anything older is dead weight. Insertion is rare
            // (once per (scope, range) per 15s), so an O(n) scan over
            // a tiny map is fine.
            //
            // `elapsed()` predicate instead of `now() - max_age`:
            // `Instant - Duration` panics if the result would predate
            // the platform zero instant, which can fire on cold-start
            // hosts where CLOCK_MONOTONIC is small. `elapsed()` is
            // saturating and safe at all uptimes.
            let max_age = TOP_USERS_TTL * 2;
            guard.retain(|_, (stored_at, _)| stored_at.elapsed() < max_age);
            guard.insert(key, (std::time::Instant::now(), resp.clone()));
        }
        Err(e) => tracing::warn!("top_users_cache mutex poisoned on insert; skipping store: {e}"),
    }
    Ok(resp)
}

async fn fetch_top_active_users_uncached(
    state: &AppState,
    user_filter: Option<&[String]>,
    range: crate::handlers::time_range::TimeRange,
) -> Result<TopActiveUsersResponse, AppError> {
    let window_start = range.window_start(chrono::Utc::now());

    if !ch_available(state) {
        return Ok(TopActiveUsersResponse {
            users: vec![],
            total: 0,
        });
    }
    // Empty team scope short-circuit, same as build_live_snapshot.
    if matches!(user_filter, Some([])) {
        return Ok(TopActiveUsersResponse {
            users: vec![],
            total: 0,
        });
    }
    let ch = ch_client(state)?;
    let from = window_start.format("%Y-%m-%d %H:%M:%S").to_string();

    // Explicit `cast(... AS String)` peels the LowCardinality + Nullable
    // wrappers off `user_id` / `user_email`: the `clickhouse` Rust crate
    // decodes a column by its declared wire type, not by what the
    // PREWHERE guarantees about its values, so a bare `SELECT user_id`
    // ships LowCardinality(Nullable(String)) encoding into a plain
    // `String` struct field and errors with "string is not valid utf8".
    //
    // The inner UNION ALL turns each row from either table into a
    // partial-credit tally (1 in one of the count columns, 0 in the
    // other) keyed by user_id. The outer GROUP BY then folds both
    // tables' contribution into a single row per user — so a caller
    // who hit ONLY MCP still ranks correctly, and a caller who hit
    // both contributes to both lanes without needing a JOIN.
    //
    // Three queries fire in parallel: the bounded leaderboard (limit
    // 50) and a uniqExact count from each of gateway_logs and
    // mcp_logs, merged with `groupArray` semantics on the Rust side
    // so `total` reflects distinct users across both kinds.
    type RowsFut = Pin<
        Box<
            dyn std::future::Future<Output = clickhouse::error::Result<Vec<TopActiveUserChRow>>>
                + Send,
        >,
    >;
    let rows_fut: RowsFut = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT \
                    cast(user_id AS String) AS user_id, \
                    cast(any(user_email) AS String) AS user_email, \
                    toUInt64(sum(api_count)) AS request_count, \
                    toInt64(sum(token_total)) AS total_tokens, \
                    toUInt64(sum(mcp_count)) AS mcp_call_count, \
                    toString(max(last_active)) AS last_active \
                 FROM ( \
                    SELECT \
                        user_id AS user_id, \
                        ifNull(user_email, '') AS user_email, \
                        toUInt64(1) AS api_count, \
                        toUInt64(0) AS mcp_count, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS token_total, \
                        created_at AS last_active \
                    FROM gateway_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL \
                    UNION ALL \
                    SELECT \
                        user_id AS user_id, \
                        ifNull(user_email, '') AS user_email, \
                        toUInt64(0) AS api_count, \
                        toUInt64(1) AS mcp_count, \
                        toInt64(0) AS token_total, \
                        created_at AS last_active \
                    FROM mcp_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL \
                 ) \
                 GROUP BY user_id \
                 ORDER BY (request_count + mcp_call_count) DESC \
                 LIMIT ?",
            )
            .bind(from.clone())
            .bind(from.clone())
            .bind(TOP_USERS_LIMIT)
            .fetch_all::<TopActiveUserChRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT \
                    cast(user_id AS String) AS user_id, \
                    cast(any(user_email) AS String) AS user_email, \
                    toUInt64(sum(api_count)) AS request_count, \
                    toInt64(sum(token_total)) AS total_tokens, \
                    toUInt64(sum(mcp_count)) AS mcp_call_count, \
                    toString(max(last_active)) AS last_active \
                 FROM ( \
                    SELECT \
                        user_id AS user_id, \
                        ifNull(user_email, '') AS user_email, \
                        toUInt64(1) AS api_count, \
                        toUInt64(0) AS mcp_count, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS token_total, \
                        created_at AS last_active \
                    FROM gateway_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL AND has(?, user_id) \
                    UNION ALL \
                    SELECT \
                        user_id AS user_id, \
                        ifNull(user_email, '') AS user_email, \
                        toUInt64(0) AS api_count, \
                        toUInt64(1) AS mcp_count, \
                        toInt64(0) AS token_total, \
                        created_at AS last_active \
                    FROM mcp_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL AND has(?, user_id) \
                 ) \
                 GROUP BY user_id \
                 ORDER BY (request_count + mcp_call_count) DESC \
                 LIMIT ?",
            )
            .bind(from.clone())
            .bind(ids)
            .bind(from.clone())
            .bind(ids)
            .bind(TOP_USERS_LIMIT)
            .fetch_all::<TopActiveUserChRow>(),
        ),
    };

    // `total` is the count of distinct users active in EITHER table
    // — same inner shape as the leaderboard query, but skipping the
    // ORDER+LIMIT and counting the grouped rows.
    type TotalFut =
        Pin<Box<dyn std::future::Future<Output = clickhouse::error::Result<u64>> + Send>>;
    let total_fut: TotalFut = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT toUInt64(count()) FROM ( \
                    SELECT user_id FROM gateway_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL \
                    UNION DISTINCT \
                    SELECT user_id FROM mcp_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL \
                 )",
            )
            .bind(from.clone())
            .bind(from.clone())
            .fetch_one::<u64>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT toUInt64(count()) FROM ( \
                    SELECT user_id FROM gateway_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL AND has(?, user_id) \
                    UNION DISTINCT \
                    SELECT user_id FROM mcp_logs \
                    PREWHERE created_at >= toDateTime(?) AND user_id IS NOT NULL AND has(?, user_id) \
                 )",
            )
            .bind(from.clone())
            .bind(ids)
            .bind(from.clone())
            .bind(ids)
            .fetch_one::<u64>(),
        ),
    };
    let (rows, total) = tokio::try_join!(rows_fut, total_fut)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("top-users CH query: {e}")))?;

    let users = rows
        .into_iter()
        .map(|r| TopActiveUserRow {
            user_id: r.user_id,
            user_email: r.user_email,
            request_count: r.request_count,
            total_tokens: r.total_tokens,
            mcp_call_count: r.mcp_call_count,
            last_active: r.last_active,
        })
        .collect();
    Ok(TopActiveUsersResponse { users, total })
}

#[cfg(test)]
mod tests {
    //! The cache fronting the top-users CH query is keyed on a hash of
    //! the RBAC filter. The hashing rules here are correctness-critical:
    //! collide two distinct scopes and a global-read user sees a
    //! team-scoped result (or vice versa).

    use super::*;

    #[test]
    fn user_filter_hash_distinguishes_none_from_empty_slice() {
        // `None` ⇒ global analytics:read_all (no SQL filter, full
        // leaderboard). `Some(&[])` ⇒ empty team scope, short-circuits
        // to an empty response. The cache MUST not conflate them.
        assert_ne!(user_filter_hash(None), user_filter_hash(Some(&[])));
    }

    #[test]
    fn user_filter_hash_is_deterministic_for_same_members() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(user_filter_hash(Some(&ids)), user_filter_hash(Some(&ids)));
    }

    #[test]
    fn user_filter_hash_distinguishes_different_membership() {
        let a = vec!["x".to_string(), "y".to_string()];
        let b = vec!["x".to_string(), "z".to_string()];
        assert_ne!(user_filter_hash(Some(&a)), user_filter_hash(Some(&b)));
    }

    #[test]
    fn user_filter_hash_is_order_insensitive() {
        // PG returns rows from `resolve_dashboard_user_filter` without
        // an ORDER BY; a plan change could reorder the same membership.
        // Without internal sort the hash would flap and invalidate the
        // cache on every revoke-tick re-resolve. Pin the invariant
        // here so a future refactor can't silently regress it.
        let asc = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let desc = vec!["c".to_string(), "b".to_string(), "a".to_string()];
        assert_eq!(user_filter_hash(Some(&asc)), user_filter_hash(Some(&desc)));
    }
}
