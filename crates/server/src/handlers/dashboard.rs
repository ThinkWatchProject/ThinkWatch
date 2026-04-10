use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::time::Duration;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::{ch_available, ch_client};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DashboardStats {
    pub total_requests_today: i64,
    pub active_providers: i64,
    pub active_api_keys: i64,
    pub connected_mcp_servers: i64,
}

#[utoipa::path(
    get,
    path = "/api/dashboard/stats",
    tag = "Dashboard",
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
) -> Result<Json<DashboardStats>, AppError> {
    let today = chrono::Utc::now().date_naive();
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

    let total_requests: Option<i64> = match &owned_teams_for_usage {
        None => {
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE created_at::date = $1")
                .bind(today)
                .fetch_one(&state.db)
                .await?
        }
        Some(team_ids) => {
            let team_ids_vec: Vec<uuid::Uuid> = team_ids.iter().copied().collect();
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_records \
                  WHERE created_at::date = $1 \
                    AND (team_id = ANY($2) OR user_id = $3)",
            )
            .bind(today)
            .bind(&team_ids_vec)
            .bind(caller_id)
            .fetch_one(&state.db)
            .await?
        }
    };

    // Filter out soft-deleted rows so the dashboard "active" tile
    // matches what the limits engine and the gateway router actually
    // see. Without `deleted_at IS NULL` the count silently inflates
    // for 30 days after a delete.
    let active_providers: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_one(&state.db)
    .await?;

    let active_api_keys: Option<i64> = match owned_teams_for_keys {
        None => {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM api_keys \
                  WHERE is_active = true AND deleted_at IS NULL AND last_used_at IS NOT NULL",
            )
            .fetch_one(&state.db)
            .await?
        }
        Some(team_ids) => {
            let team_ids_vec: Vec<uuid::Uuid> = team_ids.into_iter().collect();
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM api_keys k \
                  WHERE k.is_active = true \
                    AND k.deleted_at IS NULL \
                    AND k.last_used_at IS NOT NULL \
                    AND ( \
                        k.user_id = $1 \
                        OR EXISTS ( \
                            SELECT 1 FROM team_members tm \
                             WHERE tm.user_id = k.user_id \
                               AND tm.team_id = ANY($2) \
                        ) \
                    )",
            )
            .bind(caller_id)
            .bind(&team_ids_vec)
            .fetch_one(&state.db)
            .await?
        }
    };

    let connected_mcp_servers: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE status = 'connected'")
            .fetch_one(&state.db)
            .await?;

    Ok(Json(DashboardStats {
        total_requests_today: total_requests.unwrap_or(0),
        active_providers: active_providers.unwrap_or(0),
        active_api_keys: active_api_keys.unwrap_or(0),
        connected_mcp_servers: connected_mcp_servers.unwrap_or(0),
    }))
}

// ============================================================================
// Live dashboard panel — aggregates real metrics from ClickHouse gateway_logs
// + Postgres + the in-process circuit-breaker registry. Served two ways:
//   - GET  /api/dashboard/live  — single snapshot (first paint, fallback)
//   - WS   /api/dashboard/ws    — pushes a new snapshot every 4 s
// ============================================================================

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProviderHealth {
    /// "ai" for AI providers, "mcp" for MCP servers.
    pub kind: String,
    pub provider: String,
    pub requests: u64,
    pub avg_latency_ms: f64,
    pub success_rate: f64,
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
    pub id: String,
    pub user_id: String,
    /// model_id for "api", tool_name for "mcp".
    pub subject: String,
    /// Numeric HTTP status for "api" (e.g. "200"), or string status for
    /// "mcp" (e.g. "success" / "error").
    pub status: String,
    pub latency_ms: i64,
    /// Total tokens for "api" (input+output), 0 for "mcp".
    pub tokens: i64,
    pub created_at: String,
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
}

/// Resolve the caller's "which user_ids should be visible" filter
/// for the live dashboard. Returns:
///   - `None` — caller has `analytics:read_all` at global scope and
///     should see every gateway log (no SQL filter)
///   - `Some(user_ids)` — caller has either `analytics:read_team` at
///     team scope, or no analytics perm at all. The set always
///     contains the caller's own id, plus every team member of any
///     team the caller has `analytics:read_team` for. The list is
///     stringified because the ClickHouse `user_id` column is
///     `LowCardinality(Nullable(String))`.
///
/// This is the dashboard's own scope helper (not on AuthUser)
/// because the WebSocket loop only has a user_id from a ticket —
/// it never sees the JWT — so we can't lean on the `AuthUser`
/// helpers from auth_guard.
async fn resolve_dashboard_user_filter(
    pool: &sqlx::PgPool,
    caller_id: uuid::Uuid,
) -> Result<Option<Vec<String>>, AppError> {
    // Global analytics:read_all → no filter.
    let has_global_all: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM rbac_role_assignments ra
               JOIN rbac_roles r ON r.id = ra.role_id
              WHERE ra.user_id = $1
                AND ra.scope_kind = 'global'
                AND 'analytics:read_all' = ANY(r.permissions)
         )",
    )
    .bind(caller_id)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope check failed: {e}")))?;
    if has_global_all {
        return Ok(None);
    }

    // Otherwise build the visible-user set: caller themself + every
    // team member of any team the caller holds analytics:read_team
    // (or analytics:read_all) for at team scope.
    let user_id_strs: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT u.id::text
           FROM users u
          WHERE u.id = $1
             OR EXISTS (
                 SELECT 1 FROM team_members tm
                   JOIN rbac_role_assignments ra ON ra.scope_kind = 'team'
                                                 AND ra.scope_id = tm.team_id
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE tm.user_id = u.id
                    AND ra.user_id = $1
                    AND ('analytics:read_team' = ANY(r.permissions)
                         OR 'analytics:read_all' = ANY(r.permissions))
             )",
    )
    .bind(caller_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope query failed: {e}")))?;
    Ok(Some(user_id_strs.into_iter().map(|(s,)| s).collect()))
}

/// Build a live snapshot. Reused by both the HTTP endpoint and the WS loop.
///
/// `user_filter` is the result of `resolve_dashboard_user_filter` —
/// `None` for global admins, `Some(user_ids)` for team-scoped
/// callers (whose ClickHouse queries gain a `user_id IN (...)`
/// clause).
async fn build_live_snapshot(
    state: &AppState,
    user_filter: Option<&[String]>,
) -> Result<DashboardLive, AppError> {
    // --- Postgres queries: parallel via tokio::try_join! --------------------
    // Errors propagate so the dashboard surfaces a real failure instead of
    // pretending data is empty when the DB is down.
    //
    // `max_rpm_limit` is hard-wired to None until phase E rewires it to
    // query the new `rate_limit_rules` table. The chart still renders;
    // the reference line just won't appear.
    let providers_fut = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_all(&state.db);
    let mcp_servers_fut =
        sqlx::query_as::<_, (String,)>("SELECT name FROM mcp_servers").fetch_all(&state.db);

    let (configured_providers, configured_mcp_servers) =
        tokio::try_join!(providers_fut, mcp_servers_fut)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Dashboard PG query failed: {e}")))?;
    let max_rpm_limit: Option<i32> = None;

    // Snapshot the in-process CB registry once so we can decorate every
    // provider row with its real state below.
    let cb_states = think_watch_gateway::failover::snapshot_cb_states();

    let seed_provider = |kind: &str, name: &str| ProviderHealth {
        kind: kind.to_string(),
        provider: name.to_string(),
        requests: 0,
        avg_latency_ms: 0.0,
        success_rate: 100.0,
        cb_state: cb_states
            .get(name)
            .map(|c| c.as_str().to_string())
            .unwrap_or_else(|| "Closed".to_string()),
    };

    if !ch_available(state) {
        let mut providers: Vec<ProviderHealth> = configured_providers
            .iter()
            .map(|(name,)| seed_provider("ai", name))
            .collect();
        providers.extend(
            configured_mcp_servers
                .iter()
                .map(|(name,)| seed_provider("mcp", name)),
        );
        return Ok(DashboardLive {
            providers,
            rpm_buckets: vec![0; 30],
            recent_logs: vec![],
            max_rpm_limit,
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
                .map(|(name,)| seed_provider("ai", name))
                .chain(
                    configured_mcp_servers
                        .iter()
                        .map(|(name,)| seed_provider("mcp", name)),
                )
                .collect(),
            rpm_buckets: vec![0; 30],
            recent_logs: vec![],
            max_rpm_limit,
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
            ch.query(
                "SELECT \
                    ifNull(provider, 'unknown') AS provider, \
                    count() AS requests, \
                    avg(ifNull(latency_ms, 0)) AS avg_latency_ms, \
                    (countIf(status_code < 400) / count()) * 100 AS success_rate \
                 FROM gateway_logs \
                 WHERE created_at >= now() - INTERVAL 15 MINUTE \
                 GROUP BY provider \
                 ORDER BY requests DESC \
                 LIMIT 8",
            )
            .fetch_all::<ProviderHealthRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT \
                    ifNull(provider, 'unknown') AS provider, \
                    count() AS requests, \
                    avg(ifNull(latency_ms, 0)) AS avg_latency_ms, \
                    (countIf(status_code < 400) / count()) * 100 AS success_rate \
                 FROM gateway_logs \
                 WHERE created_at >= now() - INTERVAL 15 MINUTE \
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
                    (countIf(status = 'success') / count()) * 100 AS success_rate \
                 FROM mcp_logs \
                 WHERE created_at >= now() - INTERVAL 15 MINUTE \
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
                    (countIf(status = 'success') / count()) * 100 AS success_rate \
                 FROM mcp_logs \
                 WHERE created_at >= now() - INTERVAL 15 MINUTE \
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
                 WHERE created_at >= toStartOfMinute(now()) - INTERVAL 29 MINUTE \
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
                 WHERE created_at >= toStartOfMinute(now()) - INTERVAL 29 MINUTE \
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
    let recent_q: ChFut<LiveLogRow> = match user_filter {
        None => Box::pin(
            ch.query(
                "SELECT * FROM ( \
                    SELECT \
                        'api' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(model_id, '') AS subject, \
                        toString(ifNull(status_code, 0)) AS status, \
                        ifNull(latency_ms, 0) AS latency_ms, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS tokens, \
                        toString(created_at) AS created_at \
                    FROM gateway_logs \
                    ORDER BY created_at DESC \
                    LIMIT 20 \
                    UNION ALL \
                    SELECT \
                        'mcp' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(tool_name, '') AS subject, \
                        ifNull(status, '') AS status, \
                        ifNull(duration_ms, 0) AS latency_ms, \
                        toInt64(0) AS tokens, \
                        toString(created_at) AS created_at \
                    FROM mcp_logs \
                    ORDER BY created_at DESC \
                    LIMIT 20 \
                 ) \
                 ORDER BY created_at DESC \
                 LIMIT 16",
            )
            .fetch_all::<LiveLogRow>(),
        ),
        Some(ids) => Box::pin(
            ch.query(
                "SELECT * FROM ( \
                    SELECT \
                        'api' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(model_id, '') AS subject, \
                        toString(ifNull(status_code, 0)) AS status, \
                        ifNull(latency_ms, 0) AS latency_ms, \
                        toInt64(ifNull(input_tokens, 0)) + toInt64(ifNull(output_tokens, 0)) AS tokens, \
                        toString(created_at) AS created_at \
                    FROM gateway_logs \
                    WHERE has(?, user_id) \
                    ORDER BY created_at DESC \
                    LIMIT 20 \
                    UNION ALL \
                    SELECT \
                        'mcp' AS kind, \
                        id, \
                        ifNull(user_id, '') AS user_id, \
                        ifNull(tool_name, '') AS subject, \
                        ifNull(status, '') AS status, \
                        ifNull(duration_ms, 0) AS latency_ms, \
                        toInt64(0) AS tokens, \
                        toString(created_at) AS created_at \
                    FROM mcp_logs \
                    WHERE has(?, user_id) \
                    ORDER BY created_at DESC \
                    LIMIT 20 \
                 ) \
                 ORDER BY created_at DESC \
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
    let mut providers: Vec<ProviderHealth> = provider_rows
        .into_iter()
        .map(|r| ProviderHealth {
            kind: "ai".to_string(),
            cb_state: cb_states
                .get(&r.provider)
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "Closed".to_string()),
            provider: r.provider,
            requests: r.requests,
            avg_latency_ms: r.avg_latency_ms,
            success_rate: r.success_rate,
        })
        .collect();
    for r in mcp_rows {
        providers.push(ProviderHealth {
            kind: "mcp".to_string(),
            cb_state: cb_states
                .get(&r.provider)
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "Closed".to_string()),
            provider: r.provider,
            requests: r.requests,
            avg_latency_ms: r.avg_latency_ms,
            success_rate: r.success_rate,
        });
    }
    for (name,) in &configured_providers {
        if !providers
            .iter()
            .any(|p| p.kind == "ai" && &p.provider == name)
        {
            providers.push(seed_provider("ai", name));
        }
    }
    for (name,) in &configured_mcp_servers {
        if !providers
            .iter()
            .any(|p| p.kind == "mcp" && &p.provider == name)
        {
            providers.push(seed_provider("mcp", name));
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

    Ok(DashboardLive {
        providers,
        rpm_buckets,
        recent_logs,
        max_rpm_limit,
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
) -> Result<Json<DashboardLive>, AppError> {
    let user_filter = resolve_dashboard_user_filter(&state.db, auth_user.claims.sub).await?;
    Ok(Json(
        build_live_snapshot(&state, user_filter.as_deref()).await?,
    ))
}

// ============================================================================
// WebSocket push channel for the dashboard
//
// Browsers can't send Authorization headers on a WS upgrade. The previous
// design accepted the JWT in `?token=…`, but the URL ends up in:
//   - server access logs (full URL)
//   - reverse proxy logs
//   - browser history
//   - the Referer header on outbound links
//
// New flow:
//   1. Authenticated client POSTs `/api/dashboard/ws-ticket` to mint a
//      single-use, 30-second ticket. The ticket is bound to the user_id
//      and the access-token hash so we can still re-check JWT revocation
//      on the WS connection.
//   2. Client opens `wss://…/api/dashboard/ws?ticket=<opaque>`. The
//      handler atomically GETDELs the ticket from Redis and rejects on
//      miss.
// ============================================================================

const WS_TICKET_TTL_SECS: i64 = 30;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WsTicketResponse {
    pub ticket: String,
}

/// `POST /api/dashboard/ws-ticket` — mint a single-use ticket. Auth runs
/// via the normal `require_auth` middleware so the user proves identity
/// here without exposing the JWT in a URL afterwards.
#[utoipa::path(
    post,
    path = "/api/dashboard/ws-ticket",
    tag = "Dashboard",
    responses(
        (status = 200, description = "Single-use WebSocket ticket valid for 30 seconds", body = WsTicketResponse),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_dashboard_ws_ticket(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<WsTicketResponse>, AppError> {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let ticket = data_encoding::BASE64URL_NOPAD.encode(&bytes);
    let key = format!("dashboard_ws_ticket:{ticket}");
    // Store user_id so the WS handler knows who it's talking to without
    // re-trusting any client-supplied data. We don't bind to the JWT
    // hash here because the WS endpoint doesn't see the JWT — the
    // ticket itself is the bearer credential, with a 30s lifetime.
    let value = auth_user.claims.sub.to_string();
    let _: () = fred::interfaces::KeysInterface::set(
        &state.redis,
        &key,
        value,
        Some(fred::types::Expiration::EX(WS_TICKET_TTL_SECS)),
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to mint WS ticket: {e}")))?;
    Ok(Json(WsTicketResponse { ticket }))
}

#[derive(Debug, Deserialize)]
pub struct WsAuthQuery {
    pub ticket: Option<String>,
}

pub async fn dashboard_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(q): Query<WsAuthQuery>,
) -> Result<Response, axum::http::StatusCode> {
    // Atomically consume the ticket. Using fred's GETDEL means a replay
    // attempt always fails — the second consumer sees an empty string.
    let ticket = q.ticket.ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
    if ticket.is_empty() || ticket.len() > 64 {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    let key = format!("dashboard_ws_ticket:{ticket}");
    let user_id_str: Option<String> = fred::interfaces::KeysInterface::getdel(&state.redis, &key)
        .await
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)?;
    let user_id_str = user_id_str.ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
    let user_id: uuid::Uuid = user_id_str
        .parse()
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)?;

    // Per-user connection cap. A pathological client opening hundreds
    // of dashboard WS sockets would otherwise consume one tokio task +
    // ~4s of snapshot work each, exhausting executor / DB pool.
    let max_per_user = state.config.timeouts.dashboard_ws_max_per_user;
    if !try_acquire_ws_slot(user_id, max_per_user) {
        tracing::warn!(%user_id, "dashboard ws rejected: per-user connection cap reached");
        return Err(axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    Ok(ws.on_upgrade(move |socket| dashboard_ws_loop(socket, state, user_id)))
}

/// Redis key set by `auth.revoke_sessions` to forcibly close all live
/// dashboard WebSockets for a user. The WS loop polls this key.
pub fn user_revoked_key(user_id: uuid::Uuid) -> String {
    format!("dashboard_user_revoked:{user_id}")
}

// ---------------------------------------------------------------------------
// Per-user WS connection cap
//
// Process-local in-memory counter. We don't need cross-instance accuracy:
// each instance enforces its own cap, and the dashboard is sticky-per-tab
// so a single user typically lands on one instance anyway. The cap value
// itself comes from `Timeouts.dashboard_ws_max_per_user`.
// ---------------------------------------------------------------------------

fn ws_counts() -> &'static std::sync::Mutex<std::collections::HashMap<uuid::Uuid, usize>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<uuid::Uuid, usize>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn try_acquire_ws_slot(user_id: uuid::Uuid, max_per_user: usize) -> bool {
    let mut m = match ws_counts().lock() {
        Ok(g) => g,
        Err(_) => return true, // poisoned mutex shouldn't deny service
    };
    let cnt = m.entry(user_id).or_insert(0);
    if *cnt >= max_per_user {
        false
    } else {
        *cnt += 1;
        true
    }
}

fn release_ws_slot(user_id: uuid::Uuid) {
    if let Ok(mut m) = ws_counts().lock()
        && let Some(cnt) = m.get_mut(&user_id)
    {
        *cnt = cnt.saturating_sub(1);
        if *cnt == 0 {
            m.remove(&user_id);
        }
    }
}

/// RAII guard that releases a per-user WS slot on drop, regardless of
/// which return path the loop takes.
struct WsSlotGuard(uuid::Uuid);
impl Drop for WsSlotGuard {
    fn drop(&mut self) {
        release_ws_slot(self.0);
    }
}

async fn dashboard_ws_loop(mut socket: WebSocket, state: AppState, user_id: uuid::Uuid) {
    let _slot = WsSlotGuard(user_id);

    // Per-frame I/O ceiling. Without this a slow / dead client can hang
    // the loop indefinitely on a buffered TCP write, blocking future
    // snapshot pushes for that connection.
    let io_timeout = Duration::from_secs(state.config.timeouts.dashboard_ws_io_secs);
    let tick_secs = state.config.timeouts.dashboard_ws_tick_secs;

    // Resolve the team / user filter ONCE on connect. The filter is
    // determined by RBAC role assignments, which change rarely; we
    // accept ~24h staleness here in exchange for not querying the
    // RBAC tables on every push (every 4s × hundreds of connections
    // would dominate Postgres). A re-login picks up changes
    // immediately because the WS is closed and reopened.
    let user_filter = match resolve_dashboard_user_filter(&state.db, user_id).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(%user_id, "dashboard scope resolve failed: {e}");
            return;
        }
    };

    // Push an initial snapshot immediately so the client never sees an
    // empty UI on connect.
    if let Err(e) = push_snapshot(&mut socket, &state, user_filter.as_deref(), io_timeout).await {
        tracing::debug!("dashboard ws closed during initial push: {e}");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately — we already pushed once, so consume it.
    ticker.tick().await;

    // Re-check session revocation periodically. The auth handler's
    // revoke_sessions sets a `dashboard_user_revoked:{uid}` flag in Redis;
    // 8 ticks @ 4s = ~32s window between revoke and forced disconnect.
    let mut ticks_since_revoke_check: u32 = 0;
    const REVOKE_CHECK_EVERY: u32 = 8;
    let revoke_key = user_revoked_key(user_id);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                ticks_since_revoke_check += 1;
                if ticks_since_revoke_check >= REVOKE_CHECK_EVERY {
                    ticks_since_revoke_check = 0;
                    let revoked: u8 = fred::interfaces::KeysInterface::exists(&state.redis, &revoke_key)
                        .await
                        .unwrap_or(0);
                    if revoked > 0 {
                        tracing::info!(%user_id, "dashboard ws closing: user revoked");
                        let _ = tokio::time::timeout(
                            io_timeout,
                            socket.send(Message::Close(None)),
                        )
                        .await;
                        return;
                    }
                }
                if let Err(e) = push_snapshot(&mut socket, &state, user_filter.as_deref(), io_timeout).await {
                    tracing::debug!("dashboard ws push failed: {e}");
                    return;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Ping(p))) => {
                        // Respect the same per-frame timeout for pings.
                        match tokio::time::timeout(io_timeout, socket.send(Message::Pong(p))).await {
                            Ok(Ok(())) => {}
                            _ => return,
                        }
                    }
                    Some(Err(_)) => return,
                    _ => {} // ignore client text/binary frames
                }
            }
        }
    }
}

async fn push_snapshot(
    socket: &mut WebSocket,
    state: &AppState,
    user_filter: Option<&[String]>,
    io_timeout: Duration,
) -> Result<(), String> {
    let snap = build_live_snapshot(state, user_filter)
        .await
        .map_err(|e| format!("snapshot build failed: {e}"))?;
    let json = serde_json::to_string(&snap).map_err(|e| e.to_string())?;
    // Wrap the send in a timeout so a slow/dead client can't park us
    // here forever, blocking future pushes.
    match tokio::time::timeout(io_timeout, socket.send(Message::Text(json.into()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("ws send timed out".into()),
    }
}
