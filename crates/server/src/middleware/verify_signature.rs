use axum::{
    extract::State,
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};

use crate::app::AppState;

const HEADER_TIMESTAMP: &str = "x-signature-timestamp";
const HEADER_NONCE: &str = "x-signature-nonce";
const HEADER_SIGNATURE: &str = "x-signature";

/// Store a client-provided ECDSA P-256 public key (JWK JSON) in Redis,
/// keyed by user_id. Also stores the client IP for session binding.
/// Called from the `POST /api/auth/register-key` handler after login.
///
/// `ttl_secs` should match the session's refresh-token lifetime
/// (`jwt_refresh_ttl_days`) so the pubkey stays available for the
/// life of the session. Previously this was hard-coded to 86400 with
/// a comment claiming it "roughly matches" refresh — but refresh
/// defaults to 7 days, and after 24h the pubkey would silently
/// disappear from Redis. The verify middleware's grace branch
/// (no headers + no key → allow) then let unsigned requests through
/// unchecked, collapsing the signature-binding security model for
/// any session older than a day.
/// Outcome of an attempt to register a public key for a user.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreKeyOutcome {
    /// First registration for this user (Redis SET NX accepted).
    Stored,
    /// A key was already registered. Caller should return 409 so a
    /// stolen access cookie can't silently overwrite the
    /// legitimate user's signing key from the attacker's IP within
    /// the 120s bootstrap grace window. To rotate, the user must
    /// re-authenticate (login DELetes the keys via
    /// `issue_auth_session`) before calling register-key again.
    AlreadyExists,
}

pub async fn store_public_key(
    redis: &fred::clients::Client,
    user_id: &uuid::Uuid,
    pubkey_jwk_json: &str,
    client_ip: Option<&str>,
    ttl_secs: i64,
) -> anyhow::Result<StoreKeyOutcome> {
    let redis_key = format!("signing_pubkey:{user_id}");
    // The FIRST register-key call for this session wins; a second one
    // (potentially an attacker who captured the access cookie, racing
    // the legitimate browser) must not overwrite the key.
    //
    // `SET … NX` answers `OK` when it wrote and **nil** when it
    // refused, so the reply has to be read as an Option. Binding it to
    // `bool` looked like it expressed "stored / not stored" but cannot
    // represent nil at all: the refusal path failed to parse and became
    // a 500, which the console reported as a broken session and
    // recovered from by logging the user out. That path was unreachable
    // while this endpoint was still returning 429 for an unrelated
    // reason, so the type error sat here undetected.
    let stored: Option<String> = fred::interfaces::KeysInterface::set(
        redis,
        &redis_key,
        pubkey_jwk_json,
        Some(fred::types::Expiration::EX(ttl_secs)),
        Some(fred::types::SetOptions::NX),
        false,
    )
    .await?;
    if stored.is_none() {
        // NX refused the write — key already exists. Don't touch the
        // IP key; we'd half-update the binding (pubkey from session A,
        // IP from attacker B) which is worse than refusing entirely.
        return Ok(StoreKeyOutcome::AlreadyExists);
    }

    // Store the IP the public key was registered from for session binding
    if let Some(ip) = client_ip {
        let ip_key = format!("signing_key_ip:{user_id}");
        fred::interfaces::KeysInterface::set::<(), _, _>(
            redis,
            &ip_key,
            ip,
            Some(fred::types::Expiration::EX(ttl_secs)),
            None,
            false,
        )
        .await?;
    }

    Ok(StoreKeyOutcome::Stored)
}

/// Build the httpOnly access-token cookie. SameSite=Lax (not Strict)
/// so SSO redirects from external IdPs work — the callback request
/// is cross-site by definition.
/// Cookie name for the short-lived access token. The `__Host-` prefix
/// is a browser-enforced contract: the cookie must be Secure, have
/// Path=/, and must NOT set a Domain attribute — which collectively
/// pins it to the exact origin host and prevents any sibling subdomain
/// from reading or setting it. Spelling this out explicitly means a
/// future well-intentioned `Domain=` edit would be rejected by the
/// browser rather than silently widening the session's reach.
pub const ACCESS_COOKIE_NAME: &str = "__Host-access_token";

/// Cookie name for the longer-lived refresh token. `__Host-` would
/// require Path=/, but we scope the refresh cookie to
/// `/api/auth/refresh` so a leaky proxy log on any other path is a
/// non-event — `__Secure-` is the right prefix here (Secure required,
/// Path/Domain unconstrained by the prefix itself).
pub const REFRESH_COOKIE_NAME: &str = "__Secure-refresh_token";

pub fn access_token_cookie(token: &str, max_age_secs: i64) -> String {
    format!(
        "{ACCESS_COOKIE_NAME}={token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age_secs}"
    )
}

/// Build the httpOnly refresh-token cookie. Path scoped to
/// `/api/auth/refresh` so it's only sent on the one endpoint that
/// needs it — minimizes the blast radius if cookies leak via a
/// downstream proxy log.
pub fn refresh_token_cookie(token: &str, max_age_secs: i64) -> String {
    format!(
        "{REFRESH_COOKIE_NAME}={token}; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/refresh; Max-Age={max_age_secs}"
    )
}

/// Build the Set-Cookie values that clear the auth cookies.
/// Used by the logout handler to evict the session from the
/// browser without relying on the client to do anything.
pub fn clear_auth_cookies() -> [String; 2] {
    [
        format!("{ACCESS_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"),
        format!(
            "{REFRESH_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/refresh; Max-Age=0"
        ),
    ]
}

/// Extract a named cookie value from a `Cookie` header map.
pub fn extract_cookie_from_headers(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    let prefix = format!("{name}=");
    for cookie in cookie_header.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix(prefix.as_str())
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Extract a named cookie value from the request's `Cookie` header.
pub fn extract_cookie(
    request: &axum::http::Request<axum::body::Body>,
    name: &str,
) -> Option<String> {
    extract_cookie_from_headers(request.headers(), name)
}

/// Middleware that verifies ECDSA P-256 request signatures.
///
/// Skipped for GET/HEAD/OPTIONS. Required for POST/PUT/PATCH/DELETE.
/// Also skipped for the `POST /api/auth/register-key` endpoint (chicken-and-egg:
/// the client cannot sign before registering its public key).
///
/// Expected headers:
/// - `X-Signature-Timestamp`: Unix seconds
/// - `X-Signature-Nonce`: UUID v4
/// - `X-Signature`: `ecdsa-p256:<base64url>`
///
/// String-to-sign: `{METHOD}\n{PATH}\n{TIMESTAMP}\n{NONCE}\n{BODY_SHA256}`
pub async fn verify_signature(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip CORS preflight — browsers don't attach custom headers.
    if *request.method() == Method::OPTIONS {
        return Ok(next.run(request).await);
    }

    // Skip for API key authenticated requests — signature verification is a
    // session-security mechanism (prevents cookie theft replay). API keys are
    // a separate credential and carry their own security guarantees.
    if request
        .extensions()
        .get::<super::auth_guard::ApiKeyAuthenticated>()
        .is_some()
    {
        return Ok(next.run(request).await);
    }

    // Skip for the register-key endpoint — the client has no key to sign
    // with until this request completes (chicken-and-egg).
    if request.uri().path() == "/api/auth/register-key" {
        return Ok(next.run(request).await);
    }

    // Extract auth user from extensions (set by require_auth middleware)
    let (user_id, token_iat) = request
        .extensions()
        .get::<super::auth_guard::AuthUser>()
        .map(|u| (u.claims.sub, u.claims.iat))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // If no signature headers: check whether a public key is registered.
    // - No key registered + token freshly minted → grace window (login
    //   just happened, register-key is in flight)
    // - No key registered + token older than the grace window →
    //   reject. The pubkey TTL matches refresh lifetime, so a missing
    //   key on an aged session is "key expired or evicted", not
    //   "bootstrap in progress." The old grace branch let unsigned
    //   requests through unconditionally — combined with the prior
    //   24h pubkey TTL on 7-day refresh tokens, every session
    //   silently lost signature enforcement after a day.
    // - Key registered but no signature → reject (attacker stripping)
    const REGISTER_KEY_GRACE_SECS: i64 = 120;
    let has_sig_headers = request.headers().contains_key(HEADER_SIGNATURE);
    if !has_sig_headers {
        let has_pubkey: bool = fred::interfaces::KeysInterface::exists::<bool, _>(
            &state.redis,
            &format!("signing_pubkey:{user_id}"),
        )
        .await
        .unwrap_or(false);
        if has_pubkey {
            tracing::warn!(
                "Signature headers missing but public key registered for user {user_id}"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        let now = chrono::Utc::now().timestamp();
        let age = now.saturating_sub(token_iat);
        if age > REGISTER_KEY_GRACE_SECS {
            tracing::warn!(
                user_id = %user_id,
                age_secs = age,
                "Signature missing and pubkey expired/evicted past grace window — rejecting"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        // Within bootstrap grace window — register-key may still
        // be in flight. Allow through.
        return Ok(next.run(request).await);
    }

    // Extract signature headers (clone to release borrow on request)
    let timestamp_str = request
        .headers()
        .get(HEADER_TIMESTAMP)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let nonce = request
        .headers()
        .get(HEADER_NONCE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let signature_header = request
        .headers()
        .get(HEADER_SIGNATURE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .ok_or(StatusCode::BAD_REQUEST)?;

    // Parse timestamp and check drift
    let max_drift = state.dynamic_config.signature_drift_secs().await;
    let nonce_ttl = state.dynamic_config.signature_nonce_ttl_secs().await;

    let timestamp: i64 = timestamp_str.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > max_drift {
        tracing::warn!("Signature timestamp drift too large: {timestamp} vs {now}");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Nonce uniqueness: every request (including GET) must carry a
    // unique nonce. Since timestamp differs per request, the frontend
    // generates a fresh nonce each time, so legitimate requests never
    // collide. Replaying an intercepted request is rejected here.
    let nonce_key = format!("nonce:{user_id}:{nonce}");
    let set_result: Result<bool, _> = fred::interfaces::KeysInterface::set(
        &state.redis,
        &nonce_key,
        "1",
        Some(fred::types::Expiration::EX(nonce_ttl)),
        Some(fred::types::SetOptions::NX),
        false,
    )
    .await;

    match set_result {
        Ok(true) => {
            // Nonce was new — fall through and verify the signature.
        }
        Ok(false) => {
            // NX returned false → key already existed. This IS a real
            // replay attempt (or a buggy client reusing nonces).
            tracing::warn!("Duplicate nonce detected: {nonce}");
            return Err(StatusCode::UNAUTHORIZED);
        }
        Err(e) => {
            // Redis itself failed. Previously we collapsed this into
            // `unwrap_or(false)` which logged the misleading "Duplicate
            // nonce detected" message and sent operators chasing a
            // non-existent replay attack instead of the actual outage.
            // Fail-closed (still 401) is correct for replay safety —
            // an attacker who can DOS Redis must NOT bypass the replay
            // check — but the LOG and METRIC need to identify the real
            // cause so the alerting story works.
            metrics::counter!("signature_replay_check_redis_err_total").increment(1);
            tracing::warn!(
                error = %e,
                "Replay-check Redis SET NX failed; refusing request to fail-closed"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Get public key JWK from Redis (the single source of truth)
    let pubkey_json: Option<String> =
        fred::interfaces::KeysInterface::get(&state.redis, &format!("signing_pubkey:{user_id}"))
            .await
            .unwrap_or(None);

    let pubkey_json = pubkey_json.ok_or_else(|| {
        tracing::warn!("No signing public key found for user {user_id}");
        StatusCode::UNAUTHORIZED
    })?;

    // Session binding: validate that the request IP matches the IP the
    // public key was registered from. Fail-closed: if a session has no
    // bound IP, reject so we don't silently accept session-replay attacks.
    let ip_key = format!("signing_key_ip:{user_id}");
    let bound_ip: Option<String> = fred::interfaces::KeysInterface::get(&state.redis, &ip_key)
        .await
        .unwrap_or(None);
    let request_ip = request
        .extensions()
        .get::<super::auth_guard::AuthUser>()
        .and_then(|u| u.ip.clone())
        .unwrap_or_default();
    match (&bound_ip, request_ip.is_empty()) {
        (Some(bound), false) => {
            if bound != &request_ip {
                tracing::warn!(
                    "Signing key IP mismatch for user {user_id}: bound={bound}, request={request_ip}"
                );
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
        (None, _) => {
            tracing::warn!(
                "Signing key has no bound IP for user {user_id} — session must be re-issued"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        (Some(_), true) => {
            tracing::warn!("Signing key has bound IP but request IP unknown for user {user_id}");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Parse expected signature: ecdsa-p256:<base64url>
    let sig_b64url = signature_header
        .strip_prefix("ecdsa-p256:")
        .ok_or(StatusCode::BAD_REQUEST)?;
    let sig_bytes = data_encoding::BASE64URL_NOPAD
        .decode(sig_b64url.as_bytes())
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Buffer the body to compute hash, then reconstruct
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(request.uri().path())
        .to_string();
    let (parts, body) = request.into_parts();
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024) // 10MB max
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Compute body SHA-256
    let body_hash = hex::encode(Sha256::digest(&body_bytes));

    // Compute string-to-sign (same format as before)
    let string_to_sign = format!("{method}\n{path}\n{timestamp_str}\n{nonce}\n{body_hash}");

    // Parse public key from JWK and verify ECDSA P-256 signature
    use p256::PublicKey;
    use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};

    let public_key = PublicKey::from_jwk_str(&pubkey_json).map_err(|e| {
        tracing::error!("Failed to parse public key JWK for user {user_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let verifying_key = VerifyingKey::from(&public_key);

    // ECDSA P-256 signatures are 64 bytes (r || s) in raw/fixed format
    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        tracing::warn!("Invalid ECDSA signature format for user {user_id}: {e}");
        StatusCode::BAD_REQUEST
    })?;

    verifying_key
        .verify(string_to_sign.as_bytes(), &signature)
        .map_err(|_| {
            tracing::warn!("ECDSA signature verification failed for user {user_id}");
            StatusCode::UNAUTHORIZED
        })?;

    // Refresh the pubkey TTL on every successful verify — active
    // sessions extend the key alongside their refresh-token usage.
    // Without this, the initial TTL set at register-key time would
    // still cap key lifetime at the refresh-TTL window even for
    // continuously-active sessions; nothing extends it. Fire-and-
    // forget: a Redis hiccup here doesn't fail an otherwise-valid
    // request. `EXPIRE` is a single round-trip O(1) op.
    let pubkey_key = format!("signing_pubkey:{user_id}");
    let refresh_ttl_secs = state.dynamic_config.jwt_refresh_ttl_days().await * 86_400;
    if let Err(e) = fred::interfaces::KeysInterface::expire::<bool, _>(
        &state.redis,
        &pubkey_key,
        refresh_ttl_secs,
        None,
    )
    .await
    {
        tracing::debug!("Failed to refresh signing_pubkey TTL for user {user_id} (non-fatal): {e}");
    }

    // Reconstruct request with buffered body
    let request = Request::from_parts(parts, axum::body::Body::from(body_bytes));
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_cookie_has_required_attrs() {
        let cookie = access_token_cookie("eyJhbGciOiJIUzI1NiJ9.test", 900);
        assert!(cookie.starts_with("__Host-access_token=eyJhbGciOiJIUzI1NiJ9.test"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        // SameSite=Lax (not Strict) so SSO redirects work
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/;"));
        assert!(cookie.contains("Max-Age=900"));
        // __Host- prefix forbids a Domain attribute; spell that out so
        // a future regression trips the test instead of the browser.
        assert!(
            !cookie.to_ascii_lowercase().contains("domain="),
            "__Host- cookies must not set Domain: {cookie}"
        );
    }

    #[test]
    fn refresh_token_cookie_path_is_scoped_to_auth() {
        let cookie = refresh_token_cookie("rt-token-here", 7 * 86400);
        assert!(cookie.starts_with("__Secure-refresh_token=rt-token-here"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(
            cookie.contains("Path=/api/auth/refresh"),
            "refresh_token cookie must be scoped to /api/auth/refresh, got: {cookie}"
        );
        assert!(cookie.contains(&format!("Max-Age={}", 7 * 86400)));
    }

    #[test]
    fn clear_auth_cookies_evicts_all() {
        let cookies = clear_auth_cookies();
        assert_eq!(cookies.len(), 2);
        let joined = cookies.join("\n");
        assert!(joined.contains("__Host-access_token=;"));
        assert!(joined.contains("__Secure-refresh_token=;"));
        assert!(joined.matches("Max-Age=0").count() == 2);
        assert!(joined.contains("Path=/api/auth/refresh")); // refresh_token
        assert!(joined.contains("Path=/;")); // access_token
    }

    #[test]
    fn extract_cookie_finds_named_value() {
        let request = Request::builder()
            .header(
                "cookie",
                "session=abc; __Host-access_token=eyJ.test.sig; __Secure-refresh_token=rt-x",
            )
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_cookie(&request, ACCESS_COOKIE_NAME).as_deref(),
            Some("eyJ.test.sig")
        );
        assert_eq!(
            extract_cookie(&request, REFRESH_COOKIE_NAME).as_deref(),
            Some("rt-x")
        );
        assert_eq!(extract_cookie(&request, "missing"), None);
    }

    #[test]
    fn extract_cookie_handles_extra_whitespace() {
        let request = Request::builder()
            .header("cookie", "  access_token=val ;  other=y  ")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_cookie(&request, "access_token").as_deref(),
            Some("val")
        );
    }
}
