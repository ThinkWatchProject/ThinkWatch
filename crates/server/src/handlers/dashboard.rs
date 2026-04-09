use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::{ch_available, ch_client};

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub total_requests_today: i64,
    pub active_providers: i64,
    pub active_api_keys: i64,
    pub connected_mcp_servers: i64,
}

pub async fn get_dashboard_stats(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DashboardStats>, AppError> {
    let today = chrono::Utc::now().date_naive();

    let total_requests: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE created_at::date = $1")
            .bind(today)
            .fetch_one(&state.db)
            .await?;

    // Filter out soft-deleted rows so the dashboard "active" tile
    // matches what the limits engine and the gateway router actually
    // see. Without `deleted_at IS NULL` the count silently inflates
    // for 30 days after a delete.
    let active_providers: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_one(&state.db)
    .await?;

    let active_api_keys: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_keys WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_one(&state.db)
    .await?;

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

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize, clickhouse::Row, Deserialize)]
pub struct RpmBucket {
    pub minute: String,
    pub count: u64,
}

/// One row in the unified live feed. Sourced from either `gateway_logs`
/// (AI API requests) or `mcp_logs` (MCP tool calls), normalised so the
/// frontend can render them in a single table.
#[derive(Debug, Serialize, clickhouse::Row, Deserialize)]
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

#[derive(Debug, Serialize)]
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

/// Build a live snapshot. Reused by both the HTTP endpoint and the WS loop.
async fn build_live_snapshot(state: &AppState) -> Result<DashboardLive, AppError> {
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

    // --- Four ClickHouse queries in parallel via tokio::try_join! -----------
    // Previously these ran serially, costing 4× the CH round-trip latency
    // every WS push (every 4s). All four are independent, so parallelizing
    // is a free win.
    let providers_q = ch
        .query(
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
        .fetch_all::<ProviderHealthRow>();

    let mcp_q = ch
        .query(
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
        .fetch_all::<ProviderHealthRow>();

    let buckets_q = ch
        .query(
            "SELECT \
                toString(toStartOfMinute(created_at)) AS minute, \
                count() AS count \
             FROM gateway_logs \
             WHERE created_at >= toStartOfMinute(now()) - INTERVAL 29 MINUTE \
             GROUP BY minute \
             ORDER BY minute ASC",
        )
        .fetch_all::<RpmBucket>();

    let recent_q = ch
        .query(
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
        .fetch_all::<LiveLogRow>();

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

pub async fn get_dashboard_live(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DashboardLive>, AppError> {
    Ok(Json(build_live_snapshot(&state).await?))
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

#[derive(Debug, Serialize)]
pub struct WsTicketResponse {
    pub ticket: String,
}

/// `POST /api/dashboard/ws-ticket` — mint a single-use ticket. Auth runs
/// via the normal `require_auth` middleware so the user proves identity
/// here without exposing the JWT in a URL afterwards.
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

    // Push an initial snapshot immediately so the client never sees an
    // empty UI on connect.
    if let Err(e) = push_snapshot(&mut socket, &state, io_timeout).await {
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
                if let Err(e) = push_snapshot(&mut socket, &state, io_timeout).await {
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
    io_timeout: Duration,
) -> Result<(), String> {
    let snap = build_live_snapshot(state)
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
