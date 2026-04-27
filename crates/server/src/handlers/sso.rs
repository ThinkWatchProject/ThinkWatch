use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use hmac::{Hmac, Mac, digest::KeyInit};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use think_watch_common::audit::AuditEntry;
use think_watch_common::config::AppConfig;
use think_watch_common::crypto::parse_encryption_key;
use think_watch_common::errors::AppError;
use think_watch_common::models::User;

use crate::app::AppState;

const OIDC_STATE_KEY_PREFIX: &str = "oidc:state:";
const OIDC_STATE_TTL_SECS: i64 = 600;
const OIDC_TEST_RESULT_KEY: &str = "oidc:test:result";
const OIDC_TEST_RESULT_TTL_SECS: i64 = 1800;

/// Whether a stored authorization-flow session belongs to a real
/// login attempt (`Live`) or the wizard's verification flow (`Test`).
/// The callback uses this to decide whether to issue a JWT session
/// or simply record the test outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Live,
    Test,
}

/// Snapshot of the draft config at the moment the admin clicked "Test
/// login". Stored alongside the nonce in Redis so the callback can
/// exchange the code with the same credentials, even if the draft
/// has been edited in the meantime. The secret stays encrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfigSnapshot {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret_encrypted: String,
    pub redirect_url: String,
    pub email_claim: String,
    pub name_claim: String,
}

/// Session payload indexed by the OIDC `state` (csrf_token) in Redis.
/// One-time-use is enforced via atomic GETDEL on callback. `binding`
/// is HMAC-SHA256(encryption_key, state || ":" || nonce) and serves
/// as a cryptographic bond between the Redis key (state) and value
/// (nonce). Without it, an operator (or attacker) with Redis write
/// access could swap the nonce under a valid state's key; the HMAC
/// ensures that only a server with the encryption key can produce a
/// valid entry, so any tampering is caught at callback time.
#[derive(Serialize, Deserialize)]
pub(crate) struct OidcSessionData {
    pub(crate) nonce: String,
    pub(crate) binding: String,
    /// Defaults to `Live` for sessions written before this field
    /// existed (graceful upgrade — pre-existing in-flight logins
    /// still work after deploy).
    #[serde(default = "default_session_mode")]
    pub(crate) mode: SessionMode,
    /// Populated only for `Test` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) test_snapshot: Option<TestConfigSnapshot>,
}

fn default_session_mode() -> SessionMode {
    SessionMode::Live
}

fn state_nonce_binding(enc_key: &[u8; 32], state: &str, nonce: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(enc_key).expect("HMAC-SHA256 accepts any key length");
    mac.update(state.as_bytes());
    mac.update(b":");
    mac.update(nonce.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Persist a freshly-minted OIDC authorization session for the
/// callback to retrieve. Used by both the live `sso_authorize`
/// handler and the wizard's test-login flow.
pub(crate) async fn store_oidc_session(
    redis: &fred::clients::Client,
    config: &AppConfig,
    state_token: &str,
    nonce: &str,
    mode: SessionMode,
) -> Result<(), AppError> {
    store_oidc_session_with_snapshot(redis, config, state_token, nonce, mode, None).await
}

pub(crate) async fn store_oidc_session_with_snapshot(
    redis: &fred::clients::Client,
    config: &AppConfig,
    state_token: &str,
    nonce: &str,
    mode: SessionMode,
    snapshot: Option<TestConfigSnapshot>,
) -> Result<(), AppError> {
    let enc_key = parse_encryption_key(&config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let binding = state_nonce_binding(&enc_key, state_token, nonce);
    let session = OidcSessionData {
        nonce: nonce.to_string(),
        binding,
        mode,
        test_snapshot: snapshot,
    };
    let payload = serde_json::to_string(&session)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize oidc session: {e}")))?;
    fred::interfaces::KeysInterface::set::<(), _, _>(
        redis,
        format!("{OIDC_STATE_KEY_PREFIX}{state_token}"),
        payload,
        Some(fred::types::Expiration::EX(OIDC_STATE_TTL_SECS)),
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    Ok(())
}

/// GET /api/auth/sso/authorize — redirect to OIDC provider.
#[tracing::instrument(skip_all, fields(handler = "sso.authorize"))]
pub async fn sso_authorize(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    let (auth_url, csrf_token, nonce) = oidc.authorize_url();

    store_oidc_session(
        &state.redis,
        &state.config,
        csrf_token.secret(),
        nonce.secret(),
        SessionMode::Live,
    )
    .await?;

    Ok(Redirect::temporary(&auth_url))
}

#[derive(Deserialize)]
pub struct SsoCallbackParams {
    pub code: String,
    pub state: String,
}

/// GET /api/auth/sso/callback — handle OIDC callback for both live
/// logins and the wizard's test-login flow. The session blob's
/// `mode` field decides which branch runs.
#[tracing::instrument(skip_all, fields(handler = "sso.callback"))]
pub async fn sso_callback(
    State(state): State<AppState>,
    Query(params): Query<SsoCallbackParams>,
) -> Result<Response, AppError> {
    // Atomic retrieve + delete — enforces one-time use of the state and
    // closes the TOCTOU window where a replayed callback could re-fetch the nonce.
    let redis_key = format!("{OIDC_STATE_KEY_PREFIX}{}", params.state);
    let stored: Option<String> = fred::interfaces::KeysInterface::getdel(&state.redis, &redis_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    let stored = stored.ok_or(AppError::BadRequest("Invalid or expired SSO state".into()))?;

    let session: OidcSessionData =
        serde_json::from_str(&stored).map_err(|_| AppError::BadRequest("Invalid state".into()))?;

    // Re-derive the HMAC from (state, stored nonce) and constant-time
    // compare with the binding we stored at authorize time.
    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let expected = state_nonce_binding(&enc_key, &params.state, &session.nonce);
    if !bool::from(expected.as_bytes().ct_eq(session.binding.as_bytes())) {
        tracing::warn!(
            "OIDC state/nonce binding mismatch for state {}",
            params.state
        );
        return Err(AppError::BadRequest(
            "SSO session binding failed; please retry".into(),
        ));
    }

    let nonce = openidconnect::Nonce::new(session.nonce);

    match session.mode {
        SessionMode::Live => handle_live_callback(state, params.code, nonce).await,
        SessionMode::Test => {
            let snapshot = session.test_snapshot.ok_or(AppError::BadRequest(
                "Test session is missing config snapshot".into(),
            ))?;
            handle_test_callback(state, params.code, nonce, snapshot).await
        }
    }
}

/// Real-login branch — exchanges the code via the active manager,
/// finds-or-creates the user, and issues JWT cookies.
async fn handle_live_callback(
    state: AppState,
    code: String,
    nonce: openidconnect::Nonce,
) -> Result<Response, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    let user_info = oidc
        .exchange_code(&code, &nonce)
        .await
        .map_err(|e| AppError::BadRequest(format!("SSO authentication failed: {e}")))?;

    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE oidc_subject = $1 AND oidc_issuer = $2",
    )
    .bind(&user_info.subject)
    .bind(&user_info.issuer)
    .fetch_optional(&state.db)
    .await?;

    let user = match user {
        Some(u) => u,
        None => {
            let email = user_info.email.as_deref().unwrap_or(&user_info.subject);
            let display_name = user_info.name.as_deref().unwrap_or(email);

            let u = sqlx::query_as::<_, User>(
                r#"INSERT INTO users (email, display_name, oidc_subject, oidc_issuer)
                   VALUES ($1, $2, $3, $4) RETURNING *"#,
            )
            .bind(email)
            .bind(display_name)
            .bind(&user_info.subject)
            .bind(&user_info.issuer)
            .fetch_one(&state.db)
            .await?;

            if let Some(role_name) = state.dynamic_config.default_role().await {
                sqlx::query(
                    r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
                       SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = $2"#,
                )
                .bind(u.id)
                .bind(&role_name)
                .execute(&state.db)
                .await?;
            }

            u
        }
    };

    if !user.is_active {
        return Err(AppError::Forbidden("Account is deactivated".into()));
    }

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    let access_token = state
        .jwt
        .create_access_token_with_ttl(user.id, &user.email, access_ttl)?;
    let refresh_token =
        state
            .jwt
            .create_refresh_token_with_ttl(user.id, &user.email, refresh_ttl_days)?;

    state.audit.log(
        AuditEntry::new("auth.sso_login")
            .user_id(user.id)
            .resource("auth")
            .detail(serde_json::json!({
                "oidc_issuer": user_info.issuer,
                "oidc_subject": user_info.subject,
            })),
    );

    let frontend_url = state
        .config
        .cors_origins
        .first()
        .map(|s| s.as_str())
        .unwrap_or_else(|| {
            tracing::warn!(
                "No CORS_ORIGINS configured for SSO redirect, falling back to console address"
            );
            "/"
        });

    let redirect_url = format!("{}/#sso=ok&expires_in={}", frontend_url, access_ttl);

    use axum::http::header::{LOCATION, SET_COOKIE};
    let mut response = axum::response::Response::builder()
        .status(axum::http::StatusCode::TEMPORARY_REDIRECT)
        .body(axum::body::Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("redirect build failed: {e}")))?;
    let headers = response.headers_mut();
    if let Ok(loc) = redirect_url.parse() {
        headers.insert(LOCATION, loc);
    }
    let access_cookie =
        crate::middleware::verify_signature::access_token_cookie(&access_token, access_ttl);
    let refresh_cookie = crate::middleware::verify_signature::refresh_token_cookie(
        &refresh_token,
        refresh_ttl_days * 86400,
    );
    for cookie_str in [&access_cookie, &refresh_cookie] {
        if let Ok(v) = cookie_str.parse() {
            headers.append(SET_COOKIE, v);
        }
    }
    Ok(response)
}

/// Test-login branch — exchanges the code via the draft snapshot and
/// stashes the result in Redis instead of issuing a session. The
/// returned HTML closes the popup and broadcasts the outcome to the
/// wizard via `BroadcastChannel`.
async fn handle_test_callback(
    state: AppState,
    code: String,
    nonce: openidconnect::Nonce,
    snapshot: TestConfigSnapshot,
) -> Result<Response, AppError> {
    let client_secret = crate::oidc_helpers::decrypt_client_secret(
        &snapshot.client_secret_encrypted,
        &state.config,
    )
    .map_err(AppError::Internal)?;

    let cfg = think_watch_auth::oidc::OidcConfig {
        issuer_url: snapshot.issuer_url,
        client_id: snapshot.client_id,
        client_secret,
        redirect_url: snapshot.redirect_url,
        email_claim: snapshot.email_claim,
        name_claim: snapshot.name_claim,
    };

    let result = match think_watch_auth::oidc::OidcManager::discover(&cfg).await {
        Ok(mgr) => match mgr.exchange_code(&code, &nonce).await {
            Ok(user_info) => crate::handlers::admin::OidcTestResult {
                passed: true,
                at: chrono::Utc::now().timestamp(),
                error: None,
                claims_preview: Some(serde_json::json!({
                    "subject": user_info.subject,
                    "email": user_info.email,
                    "name": user_info.name,
                    "issuer": user_info.issuer,
                })),
            },
            Err(e) => crate::handlers::admin::OidcTestResult {
                passed: false,
                at: chrono::Utc::now().timestamp(),
                error: Some(format!("Token exchange failed: {e}")),
                claims_preview: None,
            },
        },
        Err(e) => crate::handlers::admin::OidcTestResult {
            passed: false,
            at: chrono::Utc::now().timestamp(),
            error: Some(format!("Discovery failed: {e}")),
            claims_preview: None,
        },
    };

    let payload = serde_json::to_string(&result)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize test result: {e}")))?;
    fred::interfaces::KeysInterface::set::<(), _, _>(
        &state.redis,
        OIDC_TEST_RESULT_KEY,
        payload,
        Some(fred::types::Expiration::EX(OIDC_TEST_RESULT_TTL_SECS)),
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    state.audit.log(
        AuditEntry::new(if result.passed {
            "auth.sso_test_passed"
        } else {
            "auth.sso_test_failed"
        })
        .resource("oidc")
        .detail(serde_json::json!({
            "passed": result.passed,
            "error": result.error,
        })),
    );

    let body = render_test_close_page(&result);
    Ok(Html(body).into_response())
}

/// HTML stub that runs in the popup, posts the test outcome to the
/// opener via `BroadcastChannel`, and closes itself. Render-only —
/// the canonical result is the Redis blob the wizard polls.
fn render_test_close_page(result: &crate::handlers::admin::OidcTestResult) -> String {
    let payload_js = serde_json::to_string(result).unwrap_or_else(|_| "null".to_string());
    let safe_payload = payload_js.replace("</", "<\\/");
    let status_label = if result.passed {
        "✓ Test passed"
    } else {
        "✗ Test failed"
    };
    let detail =
        result.error.as_deref().map(html_escape).unwrap_or_else(|| {
            "You can close this window and continue the setup wizard.".to_string()
        });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>SSO test result</title>
<style>
  body {{
    font-family: system-ui, -apple-system, sans-serif;
    background: #0b0d12;
    color: #e6e8ef;
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100vh;
    margin: 0;
  }}
  .card {{
    max-width: 420px;
    padding: 32px;
    border: 1px solid #1f2533;
    border-radius: 12px;
    background: #11151d;
    text-align: center;
  }}
  h1 {{ font-size: 18px; margin: 0 0 12px; }}
  p  {{ font-size: 14px; color: #a8afbe; margin: 0; }}
</style>
</head>
<body>
  <div class="card">
    <h1>{status_label}</h1>
    <p>{detail}</p>
  </div>
<script>
  (function () {{
    var payload = {safe_payload};
    try {{
      var ch = new BroadcastChannel('thinkwatch-sso-test');
      ch.postMessage(payload);
      ch.close();
    }} catch (_) {{}}
    try {{
      if (window.opener) {{
        window.opener.postMessage({{ type: 'thinkwatch-sso-test', payload: payload }}, '*');
      }}
    }} catch (_) {{}}
    setTimeout(function () {{ try {{ window.close(); }} catch (_) {{}} }}, 600);
  }})();
</script>
</body>
</html>"#
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
