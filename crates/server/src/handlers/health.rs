use axum::Json;
use axum::extract::State;
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::AppState;

pub async fn health_check() -> Json<Value> {
    Json(json!({
        "status": "ok",
    }))
}

/// GET /api/auth/sso/status — public, returns whether SSO is enabled (no auth required).
pub async fn sso_status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "enabled": state.config.oidc_enabled(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ServiceHealth {
    pub postgres: bool,
    pub redis: bool,
    pub quickwit: bool,
}

/// GET /api/health — checks connectivity to PG, Redis, Quickwit.
pub async fn api_health_check(State(state): State<AppState>) -> Json<ServiceHealth> {
    let pg_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();

    let redis_ok: bool = {
        use fred::interfaces::ClientLike;
        state.redis.ping::<String>(None).await.is_ok()
    };

    let qw_ok = if let Some(ref url) = state.config.quickwit_url {
        reqwest::get(format!("{url}/health/readyz"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    } else {
        false
    };

    Json(ServiceHealth {
        postgres: pg_ok,
        redis: redis_ok,
        quickwit: qw_ok,
    })
}
