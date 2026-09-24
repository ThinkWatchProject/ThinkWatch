use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use think_watch_auth::{api_key, password};
use think_watch_common::audit::AuditActor;
use think_watch_common::dynamic_config;
use think_watch_common::errors::AppError;
use think_watch_common::validation::{normalize_email, validate_email, validate_password};
use utoipa::ToSchema;

use crate::app::AppState;
use crate::services::setup_repository::{self as repo, FirstAdmin};

#[derive(Debug, Serialize, ToSchema)]
pub struct SetupStatusResponse {
    pub initialized: bool,
    pub needs_setup: bool,
}

/// GET /api/setup/status — public, returns initialization status.
#[utoipa::path(
    get,
    path = "/api/setup/status",
    tag = "Setup",
    responses(
        (status = 200, description = "Platform initialization status", body = SetupStatusResponse),
    ),
    security(()),
)]
pub async fn setup_status(
    State(state): State<AppState>,
) -> Result<Json<SetupStatusResponse>, AppError> {
    let initialized = state.dynamic_config.is_initialized().await;
    Ok(Json(SetupStatusResponse {
        initialized,
        needs_setup: !initialized,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetupInitRequest {
    pub admin: AdminSetup,
    pub site_name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdminSetup {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetupInitResponse {
    pub admin_id: uuid::Uuid,
    pub admin_email: String,
    pub api_key: Option<String>,
    pub message: String,
}

/// POST /api/setup/initialize — public (only works if not initialized).
/// Protected by:
/// 1. DB-level check (setup.initialized = true rejects)
/// 2. Redis rate limiting (max 5 attempts per minute per IP)
/// 3. Database advisory lock to prevent race conditions
#[utoipa::path(
    post,
    path = "/api/setup/initialize",
    tag = "Setup",
    request_body = SetupInitRequest,
    responses(
        (status = 200, description = "Setup completed — admin user and initial API key created", body = SetupInitResponse),
        (status = 400, description = "Invalid input or rate-limited"),
        (status = 403, description = "Setup already completed"),
        (status = 409, description = "Admin email already exists"),
    ),
    security(()),
)]
pub async fn setup_initialize(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, AppError> {
    // Consistent with login/register/pow-challenge: reject 400 when
    // the helper can't resolve a real IP, rather than collapsing
    // every misconfigured-proxy request into a shared "unknown"
    // bucket that an attacker could pump to lock out an honest
    // operator's one-shot initialization.
    let client_ip =
        super::auth::require_client_ip(&state, request.headers(), request.extensions()).await?;
    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let req: SetupInitRequest = super::auth::parse_json_body(request, 1024 * 1024).await?;
    // Check if already initialized (fast path from cache). Done BEFORE
    // incrementing the rate counter so a low-volume attacker can't pin
    // the bucket at 6 against an already-initialized cluster.
    if state.dynamic_config.is_initialized().await {
        return Err(AppError::Forbidden("Setup already completed".into()));
    }

    // Rate limit: max 5 setup attempts per minute per IP. Per-IP rather
    // than global so a single attacker can't lock real operators out of
    // first-boot. `unknown` is its own bucket (covers misconfigured
    // proxy / direct localhost).
    let rate_key = format!("setup_rate_limit:{client_ip}");
    // The "set the TTL only when the counter comes back as 1" spelling
    // this used to have never restores an expiry on a key that already
    // lost one — see `fixed_window`.
    let count = think_watch_common::fixed_window::incr(&state.redis, &rate_key, 60)
        .await
        .unwrap_or(1);
    if count > 5 {
        return Err(AppError::BadRequest(
            "Too many setup attempts. Please try again later.".into(),
        ));
    }

    // Double-check from DB (not cache) to prevent race condition,
    // using a PostgreSQL advisory lock to serialize concurrent attempts.
    let mut tx = state.db.begin().await?;

    // Acquire an advisory lock (key = 1 for setup). This blocks concurrent setup attempts.
    repo::lock_setup(&mut tx).await?;

    let db_initialized = repo::initialized_flag(&mut tx).await?;

    if db_initialized
        .as_ref()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(AppError::Forbidden("Setup already completed".into()));
    }

    // Validate inputs + normalize the email so the canonical
    // lowercase form lands in the DB (login normalizes on lookup
    // and would otherwise miss a mixed-case admin email).
    validate_password(&req.admin.password)?;
    let admin_email = normalize_email(&req.admin.email);
    validate_email(&admin_email)?;

    // Create the super_admin user with the first API key, and mark
    // setup done.
    let password_hash = password::hash_password(&req.admin.password)?;
    let generated = api_key::generate_api_key();
    let site_name = req.site_name.as_deref().unwrap_or("ThinkWatch");
    let admin_user = repo::create_first_admin(
        &mut tx,
        &FirstAdmin {
            email: &admin_email,
            display_name: &req.admin.display_name,
            password_hash: &password_hash,
            key_prefix: &generated.prefix,
            key_hash: &generated.hash,
            key_name: "Default Admin Key",
            key_surfaces: super::api_keys::ALLOWED_SURFACES,
            site_name,
        },
    )
    .await?;

    tx.commit().await?;

    // Reload dynamic config
    let _ = state.dynamic_config.reload().await;
    dynamic_config::notify_config_changed(&state.redis).await;

    let actor = think_watch_common::audit::AnonymousActor {
        ip: Some(&client_ip),
        user_agent: user_agent.as_deref(),
        user_email: Some(&admin_user.1),
        user_id: Some(admin_user.0),
    };
    state.audit.log(
        actor
            .audit("setup.initialize")
            .resource("system")
            .detail(serde_json::json!({
                "admin_email": admin_email,
            })),
    );

    // Auto-login: issue JWT cookies so the admin is authenticated
    // immediately after setup, without a manual login step.
    // Clear the signing-key slot first — no-op for a brand-new
    // user but keeps the clear-before-issue invariant uniform
    // across all new-session paths.
    super::auth::clear_signing_key_slot(&state.redis, admin_user.0).await;
    let session =
        super::auth::issue_auth_session(&state, admin_user.0, &admin_user.1, Some(&client_ip))
            .await?;

    use axum::response::IntoResponse;
    let mut response = Json(SetupInitResponse {
        admin_id: admin_user.0,
        admin_email: admin_user.1,
        api_key: Some(generated.plaintext),
        message: "Setup completed successfully.".into(),
    })
    .into_response();
    session.set_cookies(&mut response);
    Ok(response)
}
