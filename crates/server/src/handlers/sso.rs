use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use hmac::{Hmac, Mac, digest::KeyInit};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use think_watch_common::audit::AuditActor;
use think_watch_common::config::AppConfig;
use think_watch_common::crypto::parse_encryption_key;
use think_watch_common::errors::AppError;
use think_watch_common::models::User;

use crate::app::AppState;

const OIDC_STATE_KEY_PREFIX: &str = "oidc:state:";
const OIDC_STATE_TTL_SECS: i64 = 600;

/// Browser-binding cookie for OIDC state. The plaintext token sits in
/// the user's browser; its SHA-256 hash sits in Redis with the state
/// blob. On callback we compare the cookie's hash against the stored
/// hash — without this, the OIDC state binding was server-side-only
/// (HMAC over server-known fields), which a classic OAuth login-CSRF
/// can replay: attacker completes IdP login, lures the victim to the
/// callback URL with attacker's `code+state`, victim's browser passes
/// the server-side check and ends up holding the attacker's session.
/// Binding to a cookie the attacker can't set in the victim's browser
/// closes the loop. `__Host-` prefix + SameSite=Lax + Secure +
/// HttpOnly + Path=/ — Lax (not Strict) is required so the cookie
/// rides along on the IdP's top-level redirect back to us.
const SSO_BROWSER_COOKIE: &str = "__Host-sso_browser_token";
const SSO_BROWSER_TOKEN_BYTES: usize = 32;
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
    /// SHA-256 hex of the `__Host-sso_browser_token` cookie set on
    /// the same response as the IdP redirect. Browser-pinning closes
    /// the OAuth login-CSRF: attacker can't set the cookie in the
    /// victim's browser, so a replayed `code+state` from the attacker
    /// fails this check even though the server-side HMAC binding
    /// matches. Optional for graceful upgrade — pre-existing
    /// in-flight sessions written before this field don't carry it;
    /// the callback rejects them defensively in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) browser_binding: Option<String>,
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
    browser_binding: Option<String>,
) -> Result<(), AppError> {
    store_oidc_session_with_snapshot(
        redis,
        config,
        state_token,
        nonce,
        mode,
        None,
        browser_binding,
    )
    .await
}

pub(crate) async fn store_oidc_session_with_snapshot(
    redis: &fred::clients::Client,
    config: &AppConfig,
    state_token: &str,
    nonce: &str,
    mode: SessionMode,
    snapshot: Option<TestConfigSnapshot>,
    browser_binding: Option<String>,
) -> Result<(), AppError> {
    let enc_key = parse_encryption_key(&config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let binding = state_nonce_binding(&enc_key, state_token, nonce);
    let session = OidcSessionData {
        nonce: nonce.to_string(),
        binding,
        browser_binding,
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
pub async fn sso_authorize(
    State(state): State<AppState>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    let (auth_url, csrf_token, nonce) = oidc.authorize_url();

    // Browser-binding: mint a random token, hash it for storage,
    // ship the plaintext to the user's browser as a __Host- cookie.
    // The callback compares hashes — an attacker who initiated the
    // SSO flow can't set this cookie in the victim's browser, so a
    // replay of their `code+state` fails before we mint cookies.
    let mut raw = [0u8; SSO_BROWSER_TOKEN_BYTES];
    rand::fill(&mut raw);
    let browser_token = data_encoding::BASE64URL_NOPAD.encode(&raw);
    let browser_binding_hash = {
        let mut hasher = Sha256::new();
        hasher.update(browser_token.as_bytes());
        hex::encode(hasher.finalize())
    };

    store_oidc_session(
        &state.redis,
        &state.config,
        csrf_token.secret(),
        nonce.secret(),
        SessionMode::Live,
        Some(browser_binding_hash),
    )
    .await?;

    // SameSite=Lax (not Strict) so the IdP's top-level GET redirect
    // back to /sso/callback carries the cookie. Path=/ + Secure are
    // required by the __Host- prefix.
    let cookie = format!(
        "{SSO_BROWSER_COOKIE}={browser_token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={OIDC_STATE_TTL_SECS}"
    );
    let mut response = Redirect::temporary(&auth_url).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&cookie)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cookie header build: {e}")))?,
    );
    Ok(response)
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
    headers: axum::http::HeaderMap,
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

    // Browser binding: cookie set on `sso_authorize` must match the
    // hash stored in the Redis blob. Stops OAuth login-CSRF where an
    // attacker completes the IdP login themselves and lures the
    // victim to the callback URL with `code+state` — the victim's
    // browser doesn't have the attacker's cookie, the hashes don't
    // match, we reject. Optional in the session blob for graceful
    // upgrade, but for live mode we require it (test mode runs on
    // an admin path so the CSRF angle doesn't apply).
    if matches!(session.mode, SessionMode::Live) {
        let expected_hash = session.browser_binding.as_deref().ok_or_else(|| {
            // Pre-existing in-flight sessions from a deploy of this
            // change carry no browser_binding; reject defensively
            // rather than allow a bypass while the window drains.
            AppError::BadRequest("SSO session missing browser binding — start a fresh login".into())
        })?;
        let cookie_token = crate::middleware::verify_signature::extract_cookie_from_headers(
            &headers,
            SSO_BROWSER_COOKIE,
        )
        .ok_or_else(|| AppError::BadRequest("SSO browser binding cookie missing".into()))?;
        let actual_hash = {
            let mut hasher = Sha256::new();
            hasher.update(cookie_token.as_bytes());
            hex::encode(hasher.finalize())
        };
        if !bool::from(actual_hash.as_bytes().ct_eq(expected_hash.as_bytes())) {
            tracing::warn!("SSO browser binding mismatch on callback");
            return Err(AppError::BadRequest(
                "SSO browser binding mismatch — start a fresh login".into(),
            ));
        }
    }

    // Re-derive the HMAC from (state, stored nonce) and constant-time
    // compare with the binding we stored at authorize time.
    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let expected = state_nonce_binding(&enc_key, &params.state, &session.nonce);
    if !bool::from(expected.as_bytes().ct_eq(session.binding.as_bytes())) {
        // `params.state` has been GETDEL-consumed above, so logging it
        // here doesn't enable replay. Keep it in the message — when
        // multiple SSO logins fail concurrently the operator needs the
        // discriminator to pair the warn with the right user attempt
        // in their IdP audit feed.
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
        Some(u) if u.deleted_at.is_some() => {
            // Soft-deleted user matched the OIDC subject. The `is_active`
            // gate below catches this for accounts where soft-delete also
            // flipped is_active (which every documented delete path does),
            // but defense-in-depth: refuse explicitly here with a clear
            // 403 instead of relying on a sibling field. Falling through
            // to the None branch would also break — the (oidc_subject,
            // oidc_issuer) UNIQUE constraint would cause the INSERT to
            // fail with a confusing 500 instead of a clean 403.
            return Err(AppError::Forbidden("Account has been deleted".into()));
        }
        Some(u) => u,
        None => {
            // Pick the email to store. Priority:
            //   1. IdP-supplied email — normalized + validated. If
            //      validation rejects (non-ASCII, malformed, too
            //      long), DO NOT silently fall through to the
            //      subject branch — that would let a malicious or
            //      buggy IdP smuggle CRLF / huge strings / arbitrary
            //      UTF-8 into our `users.email` column, where they
            //      get propagated into JWT claims, MCP custom-header
            //      `{{user_email}}` substitution, and audit logs.
            //      Better: fail the SSO login loudly and refuse to
            //      provision until the IdP sends a clean email.
            //   2. No IdP email at all — synthesize a deterministic
            //      placeholder from a UUIDv5 of (issuer, subject).
            //      Stable across re-provisioning, parseable as an
            //      email, can never collide with a real address
            //      (the `.invalid` TLD is reserved by RFC 2606), and
            //      survives validate_email.
            use think_watch_common::validation::{normalize_email, validate_email};
            let email = match user_info.email.as_deref() {
                Some(raw) => {
                    let candidate = normalize_email(raw);
                    validate_email(&candidate).map_err(|_| {
                        tracing::warn!(
                            issuer = %user_info.issuer,
                            "SSO IdP supplied a malformed email; refusing to provision"
                        );
                        AppError::BadRequest(
                            "Identity provider returned an invalid email address".into(),
                        )
                    })?;
                    candidate
                }
                None => {
                    // RFC 2606 reserves `.invalid` — guaranteed never
                    // to be a real address. UUIDv5 over the
                    // (issuer, subject) pair gives us a stable
                    // identifier that re-provisioning lands on the
                    // same row.
                    let ns = uuid::Uuid::NAMESPACE_URL;
                    let id = uuid::Uuid::new_v5(
                        &ns,
                        format!("{}|{}", user_info.issuer, user_info.subject).as_bytes(),
                    );
                    format!("sso-{id}@oidc.invalid")
                }
            };
            let display_name = user_info
                .name
                .as_deref()
                .unwrap_or(user_info.email.as_deref().unwrap_or(&email));

            let u = sqlx::query_as::<_, User>(
                r#"INSERT INTO users (email, display_name, oidc_subject, oidc_issuer)
                   VALUES ($1, $2, $3, $4) RETURNING *"#,
            )
            .bind(&email)
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

    // Route through the shared session-issue path so SSO inherits
    // every invariant the password-login flow already enforces
    // (RBAC preload, cookie attribute discipline). Previously the
    // SSO branch inlined its own create_access_token /
    // create_refresh_token + cookie builders, which would silently
    // miss any future tightening of `issue_auth_session`.
    //
    // Clear the signing-key slot explicitly first — SSO is a fresh
    // login, same as the password-login path; previous session's
    // signing key is logically dead. Refresh does NOT call this
    // helper; see `clear_signing_key_slot` doc.
    super::auth::clear_signing_key_slot(&state.redis, user.id).await;
    let session = super::auth::issue_auth_session(&state, user.id, &user.email, None).await?;
    let access_ttl = session.access_ttl;

    // OIDC callback: user_id resolved from the verified token, but the
    // handler doesn't currently take a headers extractor — IP/UA stay
    // None for now. Mirrors mcp_oauth's callback pattern; a follow-up
    // can plumb headers if forensics on the SSO path matters.
    let actor = think_watch_common::audit::OAuthCallbackActor {
        user_id: user.id,
        ip: None,
        user_agent: None,
    };
    state.audit.log(
        actor
            .audit("auth.sso_login")
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

    use axum::http::header::LOCATION;
    let mut response = axum::response::Response::builder()
        .status(axum::http::StatusCode::TEMPORARY_REDIRECT)
        .body(axum::body::Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("redirect build failed: {e}")))?;
    if let Ok(loc) = redirect_url.parse() {
        response.headers_mut().insert(LOCATION, loc);
    }
    // session.set_cookies appends both Set-Cookie headers built by
    // issue_auth_session — any future cookie-attribute change there
    // applies here too without an edit on this side.
    session.set_cookies(&mut response);
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

    // SSO config-test event — the triggering admin isn't in scope at
    // this callback path. Record as a system event; the wizard-level
    // initiation audit (auth.sso_test_started, when added) would
    // capture admin attribution.
    state.audit.log(
        think_watch_common::audit::SystemActor
            .audit(if result.passed {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // html_escape — XSS defense for the OIDC wizard's test-close page
    // -----------------------------------------------------------------

    #[test]
    fn html_escape_passes_through_safe_text() {
        assert_eq!(html_escape("plain text"), "plain text");
        assert_eq!(html_escape(""), "");
    }

    #[test]
    fn html_escape_escapes_the_five_xss_chars() {
        assert_eq!(html_escape("<"), "&lt;");
        assert_eq!(html_escape(">"), "&gt;");
        assert_eq!(html_escape("\""), "&quot;");
        assert_eq!(html_escape("'"), "&#39;");
        assert_eq!(html_escape("&"), "&amp;");
    }

    #[test]
    fn html_escape_handles_ampersand_first_no_double_escape() {
        // CRITICAL: `&` MUST be escaped before `<`, otherwise the result
        // of escaping `<` to `&lt;` would itself get re-escaped to
        // `&amp;lt;`. Lock the order in.
        assert_eq!(html_escape("<a>"), "&lt;a&gt;");
        assert_eq!(html_escape("&lt;"), "&amp;lt;");
    }

    #[test]
    fn html_escape_neutralizes_classic_xss_payload() {
        // Concrete safety check: the rendered page must contain no live
        // tag after escaping a known payload.
        let payload = "<script>alert('xss')</script>";
        let out = html_escape(payload);
        assert!(!out.contains("<script>"));
        assert!(!out.contains("</script>"));
        assert_eq!(out, "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;");
    }

    #[test]
    fn html_escape_handles_attribute_breakout_attempts() {
        // `"` and `'` close attributes; both must be neutralized so a
        // value can be safely interpolated inside `attr="..."` or
        // `attr='...'`.
        assert_eq!(
            html_escape(r#"x" onerror="alert(1)"#),
            "x&quot; onerror=&quot;alert(1)"
        );
    }

    // -----------------------------------------------------------------
    // state_nonce_binding — HMAC binding state+nonce against CSRF on
    // the OIDC callback. Determinism + per-input change matter.
    // -----------------------------------------------------------------

    #[test]
    fn state_nonce_binding_is_deterministic() {
        let key = [42u8; 32];
        let a = state_nonce_binding(&key, "state1", "nonce1");
        let b = state_nonce_binding(&key, "state1", "nonce1");
        assert_eq!(a, b);
        // SHA-256 hex = 64 chars.
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn state_nonce_binding_changes_with_state() {
        let key = [42u8; 32];
        let a = state_nonce_binding(&key, "state1", "nonce1");
        let b = state_nonce_binding(&key, "state2", "nonce1");
        assert_ne!(a, b);
    }

    #[test]
    fn state_nonce_binding_changes_with_nonce() {
        let key = [42u8; 32];
        let a = state_nonce_binding(&key, "state1", "nonce1");
        let b = state_nonce_binding(&key, "state1", "nonce2");
        assert_ne!(a, b);
    }

    #[test]
    fn state_nonce_binding_changes_with_key() {
        let a = state_nonce_binding(&[1u8; 32], "state1", "nonce1");
        let b = state_nonce_binding(&[2u8; 32], "state1", "nonce1");
        assert_ne!(a, b);
    }

    #[test]
    fn state_nonce_binding_separates_state_from_nonce_via_delimiter() {
        // Without the `:` delimiter between state and nonce, splitting
        // a character from one into the other would still produce the
        // same HMAC. The `:` in the source guards against that — verify
        // by feeding shifted boundaries.
        let key = [7u8; 32];
        let a = state_nonce_binding(&key, "ab", "cd");
        let b = state_nonce_binding(&key, "a", "bcd");
        assert_ne!(a, b, "delimiter must prevent boundary-shift collision");
    }
}
