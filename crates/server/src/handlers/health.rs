use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::AppState;

pub async fn health_check() -> Json<Value> {
    Json(json!({
        "status": "ok",
    }))
}

/// GET /health/live — simple liveness probe (process is alive).
pub async fn liveness() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// GET /health/ready — readiness probe, checks critical dependencies.
///
/// Verifies the gateway can actually serve traffic, not just that
/// its dependencies respond. Includes:
///   1. Postgres reachable
///   2. Redis reachable
///   3. At least one active, non-deleted provider configured
///      (without this, every `/v1/*` request would 502 at runtime)
pub async fn readiness(State(state): State<AppState>) -> Response {
    let pg_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();

    let redis_ok: bool = {
        use fred::interfaces::ClientLike;
        state.redis.ping::<String>(None).await.is_ok()
    };

    // Provider count check — only meaningful if Postgres is up,
    // otherwise the lookup itself would fail and we'd double-count
    // the same outage.
    let providers_ready = if pg_ok {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM providers WHERE is_active = true AND deleted_at IS NULL",
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        count > 0
    } else {
        false
    };

    if pg_ok && redis_ok && providers_ready {
        Json(json!({
            "status": "ready",
            "postgres": true,
            "redis": true,
            "providers": true,
        }))
        .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "postgres": pg_ok,
                "redis": redis_ok,
                "providers": providers_ready,
            })),
        )
            .into_response()
    }
}

/// GET /api/auth/sso/status — public, returns whether SSO is enabled (no auth required).
pub async fn sso_status(State(state): State<AppState>) -> Json<Value> {
    let enabled = state.oidc.read().await.is_some();
    Json(json!({
        "enabled": enabled,
    }))
}

#[derive(Debug, Serialize)]
pub struct ServiceHealth {
    pub postgres: bool,
    pub redis: bool,
    pub clickhouse: bool,
    pub pg_latency_ms: Option<i64>,
    pub redis_latency_ms: Option<i64>,
    pub clickhouse_latency_ms: Option<i64>,
    pub pool_idle: u32,
    pub pool_active: u32,
    pub uptime_seconds: i64,
}

/// GET /api/health — detailed health check with latency info.
pub async fn api_health_check(State(state): State<AppState>) -> Response {
    // PostgreSQL
    let pg_start = std::time::Instant::now();
    let pg_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();
    let pg_latency = pg_start.elapsed().as_millis() as i64;

    // Redis
    let redis_start = std::time::Instant::now();
    let redis_ok: bool = {
        use fred::interfaces::ClientLike;
        state.redis.ping::<String>(None).await.is_ok()
    };
    let redis_latency = redis_start.elapsed().as_millis() as i64;

    // ClickHouse — use SDK client if available
    let (ch_ok, ch_latency) = if let Some(ref ch) = state.clickhouse {
        let ch_start = std::time::Instant::now();
        let ok = ch.query("SELECT 1").fetch_one::<u8>().await.is_ok();
        (ok, Some(ch_start.elapsed().as_millis() as i64))
    } else {
        (false, None)
    };

    // Pool stats
    let pool_size = state.db.size() as u64;
    let pool_idle = state.db.num_idle() as u64;

    let uptime = (chrono::Utc::now() - state.started_at).num_seconds();

    let health = ServiceHealth {
        postgres: pg_ok,
        redis: redis_ok,
        clickhouse: ch_ok,
        pg_latency_ms: if pg_ok { Some(pg_latency) } else { None },
        redis_latency_ms: if redis_ok { Some(redis_latency) } else { None },
        clickhouse_latency_ms: ch_latency,
        pool_idle: pool_idle as u32,
        pool_active: pool_size.saturating_sub(pool_idle) as u32,
        uptime_seconds: uptime,
    };

    if pg_ok && redis_ok {
        Json(health).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(health)).into_response()
    }
}
