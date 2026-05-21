//! `GET / PUT /api/dashboard/layout` — per-user persistence of
//! stat-card ordering + widget prefs. Payload shape is opaque JSON;
//! the frontend owns the schema so the widget set can change without
//! a migration every iteration.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DashboardLayout {
    pub name: String,
    pub layout_json: serde_json::Value,
}

/// GET /api/dashboard/layout — returns the caller's saved layout, or
/// `{ name: "default", layout_json: null }` if the user hasn't customized yet.
/// The frontend treats a null `layout_json` as "use built-in defaults".
#[utoipa::path(
    get,
    path = "/api/dashboard/layout",
    tag = "Dashboard",
    responses(
        (status = 200, description = "Current user's dashboard layout", body = DashboardLayout),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_dashboard_layout(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DashboardLayout>, AppError> {
    let row: Option<(String, serde_json::Value)> =
        sqlx::query_as("SELECT name, layout_json FROM user_dashboard_layouts WHERE user_id = $1")
            .bind(auth_user.claims.sub)
            .fetch_optional(&state.db)
            .await?;

    let (name, layout_json) = row.unwrap_or_else(|| ("default".into(), serde_json::Value::Null));
    Ok(Json(DashboardLayout { name, layout_json }))
}

/// PUT /api/dashboard/layout — upsert the caller's layout. Payload shape is
/// intentionally opaque JSON; the frontend owns the schema so we can iterate
/// on the widget set without a migration every time.
#[utoipa::path(
    put,
    path = "/api/dashboard/layout",
    tag = "Dashboard",
    request_body = DashboardLayout,
    responses(
        (status = 200, description = "Layout saved"),
        (status = 400, description = "Payload too large"),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_token" = []))
)]
pub async fn put_dashboard_layout(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<DashboardLayout>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Cap payload at 16 KB — layouts are a list of ~10 card ids plus a few
    // bytes of settings. Anything bigger is either abuse or a bug on the
    // client that would fill our DB with garbage.
    let serialized = serde_json::to_vec(&req.layout_json)
        .map_err(|e| AppError::BadRequest(format!("Invalid layout_json: {e}")))?;
    if serialized.len() > 16 * 1024 {
        return Err(AppError::BadRequest(
            "layout_json exceeds 16KB limit".into(),
        ));
    }
    let name = if req.name.is_empty() {
        "default".to_owned()
    } else if req.name.chars().count() > 64 {
        // Count codepoints, not bytes — the message says "chars" and
        // a 64-codepoint CJK / emoji name would be 192+ bytes and
        // hit the byte-length check after only ~21 characters.
        return Err(AppError::BadRequest("name must be ≤ 64 chars".into()));
    } else {
        req.name
    };

    sqlx::query(
        "INSERT INTO user_dashboard_layouts (user_id, name, layout_json, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (user_id) DO UPDATE \
           SET name = EXCLUDED.name, \
               layout_json = EXCLUDED.layout_json, \
               updated_at = now()",
    )
    .bind(auth_user.claims.sub)
    .bind(&name)
    .bind(&req.layout_json)
    .execute(&state.db)
    .await?;

    Ok(Json(serde_json::json!({ "status": "saved" })))
}
