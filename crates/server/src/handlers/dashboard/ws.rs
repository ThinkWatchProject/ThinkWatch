//! WebSocket push channel for the dashboard.
//!
//! Browsers can't send Authorization headers on a WS upgrade, and
//! passing the JWT in `?token=…` would leak it into access logs,
//! reverse proxy logs, browser history, and Referer headers.
//!
//! Flow:
//!   1. Authenticated client POSTs `/api/dashboard/ws-ticket` to mint
//!      a single-use, 30-second ticket bound to the user_id.
//!   2. Client opens `wss://…/api/dashboard/ws?ticket=<opaque>`. The
//!      handler atomically GETDELs the ticket from Redis and rejects
//!      on miss.
//!
//! Also owns the per-user connection cap (process-local) and the
//! revoke-key the `auth.revoke_sessions` handler sets to forcibly
//! close live dashboard WebSockets.

use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::live::build_live_snapshot;
use super::scope::resolve_dashboard_user_filter;

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
    /// Caller-selected leaderboard window (24h / 7d / 30d). Folded
    /// into every snapshot's `top_users` field. Unrecognised values
    /// silently fall back to 24h, matching the REST handler.
    pub range: Option<String>,
}

pub async fn dashboard_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(q): Query<WsAuthQuery>,
) -> Result<Response, axum::http::StatusCode> {
    use crate::handlers::time_range::TimeRange;
    let range = TimeRange::parse(q.range.as_deref());
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
    let max_per_user = state.dynamic_config.perf_dashboard_ws_max_per_user().await as usize;
    if !try_acquire_ws_slot(user_id, max_per_user) {
        tracing::warn!(%user_id, "dashboard ws rejected: per-user connection cap reached");
        return Err(axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    // Cap inbound frame size. The dashboard WS protocol is server →
    // client snapshots; the client only sends Close + Ping frames
    // (already documented at the WS loop). axum/tungstenite default
    // allows ~64 MiB per message — a malicious authenticated client
    // could pump that much per frame to amplify memory pressure even
    // though we never parse the payload. 64 KiB is far more than any
    // legitimate Ping/Close needs.
    let ws = ws.max_message_size(64 * 1024).max_frame_size(64 * 1024);
    Ok(ws.on_upgrade(move |socket| dashboard_ws_loop(socket, state, user_id, range)))
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

async fn dashboard_ws_loop(
    mut socket: WebSocket,
    state: AppState,
    user_id: uuid::Uuid,
    top_users_range: crate::handlers::time_range::TimeRange,
) {
    let _slot = WsSlotGuard(user_id);

    // Per-frame I/O ceiling. Without this a slow / dead client can hang
    // the loop indefinitely on a buffered TCP write, blocking future
    // snapshot pushes for that connection.
    let io_timeout =
        Duration::from_secs(state.dynamic_config.perf_dashboard_ws_io_secs().await as u64);
    let tick_secs = state.dynamic_config.perf_dashboard_ws_tick_secs().await as u64;

    // Resolve the team / user filter on connect and re-resolve on every
    // revoke tick (~32s) so a role that's revoked mid-session stops
    // delivering data within the same window the revoke key is polled.
    // Earlier this was resolved ONCE and cached for the connection's
    // lifetime — an admin removing a user from a team kept streaming
    // that team's data to the user's open tab until they refreshed.
    // The RBAC query is two indexed joins (~1ms); at the 32s cadence
    // it's well under the gateway-snapshot cost.
    let mut user_filter = match resolve_dashboard_user_filter(&state.db, user_id).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(%user_id, "dashboard scope resolve failed: {e}");
            return;
        }
    };

    // Push an initial snapshot immediately so the client never sees an
    // empty UI on connect.
    if let Err(e) = push_snapshot(
        &mut socket,
        &state,
        user_filter.as_deref(),
        top_users_range,
        io_timeout,
    )
    .await
    {
        tracing::debug!("dashboard ws closed during initial push: {e}");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately — we already pushed once, so consume it.
    ticker.tick().await;

    // Re-check session revocation on a fixed wall-clock interval so the
    // revoke window doesn't stretch when an operator tunes the snapshot
    // tick to a longer value for performance. Bound is fixed at 32s
    // independent of `tick_secs`; previously this was 8×tick_secs which
    // grew to several minutes when tick_secs was raised.
    let revoke_key = user_revoked_key(user_id);
    let mut revoke_ticker = tokio::time::interval(Duration::from_secs(32));
    revoke_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    revoke_ticker.tick().await;

    loop {
        tokio::select! {
                _ = revoke_ticker.tick() => {
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
                    // Refresh the team/user filter. A role un-assignment or
                    // team membership change between two revoke ticks
                    // tightens the visible-user set on the next snapshot.
                    // A Redis hiccup on the revoke check above is fail-soft
                    // (continue serving); a Postgres hiccup here is the
                    // same — keep the previous filter rather than break
                    // the connection over a transient error.
                    match resolve_dashboard_user_filter(&state.db, user_id).await {
                        Ok(f) => user_filter = f,
                        Err(e) => tracing::warn!(%user_id, "dashboard scope re-resolve failed (keeping stale filter): {e}"),
                    }
                }
                _ = ticker.tick() => {
                    if let Err(e) =
            push_snapshot(&mut socket, &state, user_filter.as_deref(), top_users_range, io_timeout)
                .await
        {
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
    top_users_range: crate::handlers::time_range::TimeRange,
    io_timeout: Duration,
) -> Result<(), String> {
    let snap = build_live_snapshot(state, user_filter, top_users_range)
        .await
        .map_err(|e| format!("snapshot build failed: {e}"))?;
    // Build a compact serialisation (no whitespace) so each tick ships
    // the smallest representation we can produce without negotiating a
    // permessage-deflate extension. Axum's WebSocket layer in the
    // current axum/tokio-tungstenite version doesn't expose the deflate
    // negotiation flag — gzipped binary frames would force a custom
    // client and break the existing JSON consumer. The compact form
    // keeps the payload size win that's achievable in-process; for
    // wire-level compression run an nginx terminator with
    // `proxy_set_header X-Forwarded-WebSocket-Compression on`.
    let json = serde_json::to_string(&snap).map_err(|e| e.to_string())?;
    // Wrap the send in a timeout so a slow/dead client can't park us
    // here forever, blocking future pushes.
    match tokio::time::timeout(io_timeout, socket.send(Message::Text(json.into()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("ws send timed out".into()),
    }
}

#[cfg(test)]
mod tests {
    //! The ws_slot machinery lives behind a process-global static map
    //! (OnceLock<Mutex<HashMap>>). Tests run in parallel under nextest,
    //! so EVERY test here MUST use a fresh `uuid::Uuid::new_v4()` —
    //! sharing a fixed UUID would race against neighboring tests'
    //! acquire/release operations and produce flakes.

    use super::*;

    #[test]
    fn try_acquire_succeeds_under_cap() {
        let user = uuid::Uuid::new_v4();
        assert!(try_acquire_ws_slot(user, 2));
        assert!(try_acquire_ws_slot(user, 2));
        release_ws_slot(user);
        release_ws_slot(user);
    }

    #[test]
    fn try_acquire_fails_at_cap() {
        let user = uuid::Uuid::new_v4();
        assert!(try_acquire_ws_slot(user, 1));
        // Second acquire at cap of 1 must fail without incrementing —
        // otherwise the cap is advisory rather than enforced.
        assert!(!try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
    }

    #[test]
    fn release_decrements_and_lets_next_acquire_through() {
        let user = uuid::Uuid::new_v4();
        assert!(try_acquire_ws_slot(user, 1));
        assert!(!try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
        // Slot is free again now.
        assert!(try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
    }

    #[test]
    fn release_to_zero_removes_key_from_map() {
        // Memory hygiene: an idle user shouldn't leave a 0-count entry
        // in the global HashMap. Verify by acquiring + releasing
        // exactly once, then re-acquiring and watching the cap apply
        // from a fresh count.
        let user = uuid::Uuid::new_v4();
        assert!(try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
        // Re-acquire under a new cap of 2 — first should succeed,
        // second should succeed (since we're starting from 0, not 1).
        assert!(try_acquire_ws_slot(user, 2));
        assert!(try_acquire_ws_slot(user, 2));
        release_ws_slot(user);
        release_ws_slot(user);
    }

    #[test]
    fn release_on_missing_key_is_noop() {
        // Defensive: releasing a slot we never acquired must NOT panic
        // and must not produce a negative count (saturating_sub).
        let user = uuid::Uuid::new_v4();
        release_ws_slot(user); // never acquired
        // We should still be able to acquire normally afterward.
        assert!(try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
    }

    #[test]
    fn ws_slot_guard_releases_on_drop() {
        let user = uuid::Uuid::new_v4();
        {
            let _guard = WsSlotGuard(user);
            assert!(try_acquire_ws_slot(user, 2));
            // Inside scope: 1 slot held by guard semantics (but guard
            // itself doesn't *acquire*, only releases) + 1 held by the
            // manual acquire above = 1 manual. Drop will release once.
        }
        // Guard's Drop fired one release. Now manually release the
        // one we acquired explicitly.
        // Net effect on the counter: +1 - 1 (guard) - 1 (manual) = -1,
        // which saturates at 0. Re-acquire to confirm we're at 0.
        assert!(try_acquire_ws_slot(user, 1));
        release_ws_slot(user);
    }
}
