//! Request-trace endpoint — joins events across gateway_logs,
//! mcp_logs, and audit_logs by trace_id. Gives operators a single
//! chronological view of everything that happened for one request.
//!
//! The trace_id itself is the gateway's `metadata.request_id` — returned
//! to clients in the `X-Metadata-Request-Id` response header, so a
//! support ticket with that header can be pasted straight into the
//! trace URL.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::{ch_available, ch_client};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TraceEvent {
    /// "gateway" | "mcp" | "audit" — lets the frontend colour / icon by kind.
    pub kind: String,
    pub id: String,
    /// RFC3339 timestamp. The full event list is returned sorted by this.
    pub created_at: String,
    /// Short human-facing label — model_id for gateway, tool_name for mcp,
    /// action for audit. Empty string if the underlying column is NULL.
    pub subject: String,
    /// Numeric HTTP status for gateway rows; MCP status string; empty for audit.
    pub status: String,
    /// Duration for gateway+mcp rows (ms); 0 for audit events.
    pub duration_ms: i64,
    /// Optional user_id, if the row has one.
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TraceResponse {
    pub trace_id: String,
    /// Events sorted oldest → newest. Empty when no rows match.
    pub events: Vec<TraceEvent>,
}

#[utoipa::path(
    get,
    path = "/api/admin/trace/{trace_id}",
    tag = "Admin",
    params(("trace_id" = String, Path, description = "Correlation id (metadata.request_id)")),
    responses(
        (status = 200, description = "All events tagged with this trace_id", body = TraceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_trace(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(trace_id): Path<String>,
) -> Result<Json<TraceResponse>, AppError> {
    auth_user.require_permission("analytics:read_all")?;
    auth_user
        .assert_scope_global(&state.db, "analytics:read_all")
        .await?;

    // Per-admin rate limit: 60 lookups/min. Generous enough that the
    // page can poll every couple seconds, tight enough that a stolen
    // admin token can't run a CH-hammering script. Best-effort against
    // a Redis outage so the admin can still investigate during partial
    // failures.
    super::test_rate_limit::check_admin_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "trace_lookup",
        60,
    )
    .await?;

    // Basic sanity: trace_ids are UUIDs in practice but we accept any
    // short token to avoid coupling to a particular format. Length cap
    // keeps a path-segment abuse from fanning out into CH unbounded.
    if trace_id.is_empty() || trace_id.len() > 128 {
        return Err(AppError::BadRequest(
            "trace_id must be 1–128 characters".into(),
        ));
    }

    let mut events: Vec<TraceEvent> = Vec::new();

    if ch_available(&state) {
        let ch = ch_client(&state)?;

        // Each of the three log tables gets queried independently — the
        // trace_id bloom-filter index makes this cheap, and union'ing
        // in CH would require matching column lists which we don't have.

        #[derive(clickhouse::Row, serde::Deserialize)]
        struct GatewayRow {
            id: String,
            created_at: String,
            model_id: Option<String>,
            status_code: Option<i64>,
            latency_ms: Option<i64>,
            user_id: Option<String>,
        }
        let rows: Vec<GatewayRow> = ch
            .query(
                "SELECT id, \
                        formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                        model_id, status_code, latency_ms, user_id \
                   FROM gateway_logs \
                  WHERE trace_id = ?",
            )
            .bind(&trace_id)
            .fetch_all::<GatewayRow>()
            .await
            .unwrap_or_default();
        for r in rows {
            events.push(TraceEvent {
                kind: "gateway".into(),
                id: r.id,
                created_at: r.created_at,
                subject: r.model_id.unwrap_or_default(),
                status: r.status_code.map(|c| c.to_string()).unwrap_or_default(),
                duration_ms: r.latency_ms.unwrap_or(0),
                user_id: r.user_id,
            });
        }

        #[derive(clickhouse::Row, serde::Deserialize)]
        struct McpRow {
            id: String,
            created_at: String,
            tool_name: Option<String>,
            status: Option<String>,
            duration_ms: Option<i64>,
            user_id: Option<String>,
        }
        let rows: Vec<McpRow> = ch
            .query(
                "SELECT id, \
                        formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                        tool_name, status, duration_ms, user_id \
                   FROM mcp_logs \
                  WHERE trace_id = ?",
            )
            .bind(&trace_id)
            .fetch_all::<McpRow>()
            .await
            .unwrap_or_default();
        for r in rows {
            events.push(TraceEvent {
                kind: "mcp".into(),
                id: r.id,
                created_at: r.created_at,
                subject: r.tool_name.unwrap_or_default(),
                status: r.status.unwrap_or_default(),
                duration_ms: r.duration_ms.unwrap_or(0),
                user_id: r.user_id,
            });
        }

        #[derive(clickhouse::Row, serde::Deserialize)]
        struct AuditRow {
            id: String,
            created_at: String,
            action: String,
            user_id: Option<String>,
        }
        let rows: Vec<AuditRow> = ch
            .query(
                "SELECT id, \
                        formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                        action, user_id \
                   FROM audit_logs \
                  WHERE trace_id = ?",
            )
            .bind(&trace_id)
            .fetch_all::<AuditRow>()
            .await
            .unwrap_or_default();
        for r in rows {
            events.push(TraceEvent {
                kind: "audit".into(),
                id: r.id,
                created_at: r.created_at,
                subject: r.action,
                status: String::new(),
                duration_ms: 0,
                user_id: r.user_id,
            });
        }

        // Best-effort app_logs correlation. The proxy emits tracing
        // events like `tracing::info!(request_id = %trace_id, …)` so
        // the trace id lands in the structured `fields` JSON column.
        // We do a substring match against `fields` and `span` bounded
        // to the last 1h — a full-period scan would hammer CH.
        // Operators investigating older requests should use the
        // dedicated /logs explorer.
        //
        // When a future schema adds first-class `app_logs.trace_id`,
        // swap the LIKE for an indexed equality predicate.
        #[derive(clickhouse::Row, serde::Deserialize)]
        struct AppLogRow {
            id: String,
            created_at: String,
            level: String,
            message: String,
        }
        let pattern = format!("%{trace_id}%");
        let app_rows: Vec<AppLogRow> = ch
            .query(
                "SELECT id, \
                        formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                        level, message \
                   FROM app_logs \
                  WHERE created_at >= now() - INTERVAL 1 HOUR \
                    AND (fields LIKE ? OR span LIKE ?) \
                  LIMIT 200",
            )
            .bind(&pattern)
            .bind(&pattern)
            .fetch_all::<AppLogRow>()
            .await
            .unwrap_or_default();
        for r in app_rows {
            events.push(TraceEvent {
                kind: "app".into(),
                id: r.id,
                created_at: r.created_at,
                // Truncate the message so a stack-trace-style log
                // doesn't blow up the timeline cell. Full text is in
                // /logs explorer if the operator needs it.
                subject: r.message.chars().take(120).collect::<String>(),
                status: r.level,
                duration_ms: 0,
                user_id: None,
            });
        }
    }

    events.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    Ok(Json(TraceResponse { trace_id, events }))
}
