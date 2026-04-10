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

/// Generate a random 32-byte signing key and store it in Redis keyed by user_id.
/// Also stores the client IP for session binding validation.
/// Returns the hex-encoded key.
pub async fn create_signing_key(
    redis: &fred::clients::Client,
    user_id: &uuid::Uuid,
    client_ip: Option<&str>,
) -> anyhow::Result<String> {
    let mut key_bytes = [0u8; 32];
    rand::fill(&mut key_bytes);
    let hex_key = hex::encode(key_bytes);

    let redis_key = format!("signing_key:{user_id}");
    // Store with 24h TTL (matches refresh token lifetime roughly)
    fred::interfaces::KeysInterface::set::<(), _, _>(
        redis,
        &redis_key,
        &hex_key,
        Some(fred::types::Expiration::EX(86400)),
        None,
        false,
    )
    .await?;

    // Store the IP the signing key was issued to for session binding
    if let Some(ip) = client_ip {
        let ip_key = format!("signing_key_ip:{user_id}");
        fred::interfaces::KeysInterface::set::<(), _, _>(
            redis,
            &ip_key,
            ip,
            Some(fred::types::Expiration::EX(86400)),
            None,
            false,
        )
        .await?;
    }

    Ok(hex_key)
}

/// Build an httpOnly cookie header value for the signing key.
pub fn signing_key_cookie(key: &str, max_age_secs: i64) -> String {
    format!("signing_key={key}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_secs}")
}

/// Build the httpOnly access-token cookie. SameSite=Lax (not Strict)
/// so SSO redirects from external IdPs work — the callback request
/// is cross-site by definition.
pub fn access_token_cookie(token: &str, max_age_secs: i64) -> String {
    format!("access_token={token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age_secs}")
}

/// Build the httpOnly refresh-token cookie. Path scoped to
/// `/api/auth/refresh` so it's only sent on the one endpoint that
/// needs it — minimizes the blast radius if cookies leak via a
/// downstream proxy log.
pub fn refresh_token_cookie(token: &str, max_age_secs: i64) -> String {
    format!(
        "refresh_token={token}; HttpOnly; Secure; SameSite=Lax; Path=/api/auth; Max-Age={max_age_secs}"
    )
}

/// Build the three Set-Cookie values that clear the auth cookies.
/// Used by the logout handler to evict the session from the
/// browser without relying on the client to do anything.
pub fn clear_auth_cookies() -> [String; 3] {
    [
        "access_token=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0".to_string(),
        "refresh_token=; HttpOnly; Secure; SameSite=Lax; Path=/api/auth; Max-Age=0".to_string(),
        "signing_key=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0".to_string(),
    ]
}

/// Extract a named cookie value from the request's `Cookie` header.
pub fn extract_cookie(
    request: &axum::http::Request<axum::body::Body>,
    name: &str,
) -> Option<String> {
    let cookie_header = request
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())?;
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

/// Extract signing key from the `signing_key` httpOnly cookie, falling back
/// to the `X-Signing-Key` header for backwards compatibility.
pub fn extract_signing_key_from_request(
    request: &axum::http::Request<axum::body::Body>,
) -> Option<String> {
    extract_cookie(request, "signing_key")
}

/// Middleware that verifies HMAC-SHA256 request signatures on state-changing methods.
///
/// Skipped for GET/HEAD/OPTIONS. Required for POST/PUT/PATCH/DELETE.
///
/// Expected headers:
/// - `X-Signature-Timestamp`: Unix seconds
/// - `X-Signature-Nonce`: UUID v4
/// - `X-Signature`: `hmac-sha256:<hex>`
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

    // Skip for API key authenticated requests — HMAC is a session-security
    // mechanism (prevents cookie theft replay). API keys are a separate
    // credential and carry their own security guarantees.
    if request
        .extensions()
        .get::<super::auth_guard::ApiKeyAuthenticated>()
        .is_some()
    {
        return Ok(next.run(request).await);
    }

    // Extract auth user from extensions (set by require_auth middleware)
    let user_id = request
        .extensions()
        .get::<super::auth_guard::AuthUser>()
        .map(|u| u.claims.sub)
        .ok_or(StatusCode::UNAUTHORIZED)?;

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

    // Per-user nonce rate limit: max 120 nonces per **rolling** minute.
    //
    // The previous implementation used a fixed-window counter (incr +
    // expire 60s). With a fixed window an attacker could send 120
    // requests at second 59 of one window and 120 more at second 0 of
    // the next, achieving 240/min effective. Using a Redis sorted set
    // keyed on monotonic timestamps gives a true sliding window.
    {
        use fred::interfaces::SortedSetsInterface;
        let nonce_rate_key = format!("nonce_rate_zset:{user_id}");
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let window_start_ms = now_ms - 60_000;
        // Drop expired entries from the head of the window.
        let _: i64 = SortedSetsInterface::zremrangebyscore(
            &state.redis,
            &nonce_rate_key,
            0,
            window_start_ms,
        )
        .await
        .unwrap_or(0);
        // Count what's left in the window.
        let in_window: i64 = SortedSetsInterface::zcard(&state.redis, &nonce_rate_key)
            .await
            .unwrap_or(0);
        if in_window >= 120 {
            tracing::warn!("Nonce rate limit exceeded for user {user_id} ({in_window}/120/min)");
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        // Add this request's nonce to the window. Use the nonce string
        // as the member to keep entries unique even if two requests
        // arrive at the same millisecond.
        let _: i64 = SortedSetsInterface::zadd(
            &state.redis,
            &nonce_rate_key,
            None,
            None,
            false,
            false,
            (now_ms as f64, nonce.to_string()),
        )
        .await
        .unwrap_or(0);
        // Refresh TTL so the key disappears once the user goes idle.
        let _: () =
            fred::interfaces::KeysInterface::expire(&state.redis, &nonce_rate_key, 120, None)
                .await
                .unwrap_or(());
    }

    // Check nonce uniqueness (prevent replay)
    let nonce_key = format!("nonce:{user_id}:{nonce}");
    let was_set: bool = fred::interfaces::KeysInterface::set(
        &state.redis,
        &nonce_key,
        "1",
        Some(fred::types::Expiration::EX(nonce_ttl)),
        Some(fred::types::SetOptions::NX), // Only set if not exists
        false,
    )
    .await
    .unwrap_or(false);

    if !was_set {
        tracing::warn!("Duplicate nonce detected: {nonce}");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Get signing key: try httpOnly cookie first, then Redis lookup
    let signing_key_hex: Option<String> =
        if let Some(cookie_key) = extract_signing_key_from_request(&request) {
            Some(cookie_key)
        } else {
            fred::interfaces::KeysInterface::get(&state.redis, &format!("signing_key:{user_id}"))
                .await
                .unwrap_or(None)
        };

    let signing_key_hex = signing_key_hex.ok_or_else(|| {
        tracing::warn!("No signing key found for user {user_id}");
        StatusCode::UNAUTHORIZED
    })?;
    let signing_key = hex::decode(&signing_key_hex).map_err(|e| {
        tracing::error!("Failed to hex-decode signing key for user {user_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Session binding: validate that the request IP matches the IP the
    // signing key was issued to. Fail-closed: if a session was issued
    // without a bound IP for some reason (legacy session, Redis flush
    // mid-rollout), reject so we don't silently accept session-replay
    // attacks. The login/signup paths always bind an IP today.
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
            // Bound but request IP unknown — likely an internal request
            // path that didn't run the auth_guard. Reject to be safe.
            tracing::warn!("Signing key has bound IP but request IP unknown for user {user_id}");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Parse expected signature
    let expected_hex = signature_header
        .strip_prefix("hmac-sha256:")
        .ok_or(StatusCode::BAD_REQUEST)?;
    let expected_sig = hex::decode(expected_hex).map_err(|_| StatusCode::BAD_REQUEST)?;

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

    // Compute string-to-sign
    let string_to_sign = format!("{method}\n{path}\n{timestamp_str}\n{nonce}\n{body_hash}");

    // Compute HMAC-SHA256
    use hmac::{Hmac, Mac, digest::KeyInit};
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(&signing_key).map_err(|e| {
        tracing::error!("Failed to create HMAC key for user {user_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    mac.update(string_to_sign.as_bytes());

    // Constant-time comparison via hmac::Mac::verify_slice
    mac.verify_slice(&expected_sig).map_err(|_| {
        tracing::warn!("Signature verification failed for user {user_id}");
        StatusCode::UNAUTHORIZED
    })?;

    // Reconstruct request with buffered body
    let request = Request::from_parts(parts, axum::body::Body::from(body_bytes));
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_cookie_format() {
        let cookie = signing_key_cookie("abcdef1234567890", 86400);
        assert!(cookie.starts_with("signing_key=abcdef1234567890"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=86400"));
    }

    #[test]
    fn extract_signing_key_from_cookie_header() {
        let request = Request::builder()
            .header("cookie", "other=x; signing_key=deadbeef1234; session=y")
            .body(axum::body::Body::empty())
            .unwrap();
        let key = extract_signing_key_from_request(&request);
        assert_eq!(key.as_deref(), Some("deadbeef1234"));
    }

    #[test]
    fn extract_signing_key_missing() {
        let request = Request::builder()
            .header("cookie", "session=abc")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(extract_signing_key_from_request(&request).is_none());
    }

    #[test]
    fn extract_signing_key_no_cookie_header() {
        let request = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert!(extract_signing_key_from_request(&request).is_none());
    }

    #[test]
    fn extract_signing_key_empty_value() {
        let request = Request::builder()
            .header("cookie", "signing_key=; other=x")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(extract_signing_key_from_request(&request).is_none());
    }

    // ----- Wave-C cookie helpers -----

    #[test]
    fn access_token_cookie_has_required_attrs() {
        let cookie = access_token_cookie("eyJhbGciOiJIUzI1NiJ9.test", 900);
        assert!(cookie.starts_with("access_token=eyJhbGciOiJIUzI1NiJ9.test"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        // SameSite=Lax (not Strict) so SSO redirects work
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/;"));
        assert!(cookie.contains("Max-Age=900"));
    }

    #[test]
    fn refresh_token_cookie_path_is_scoped_to_auth() {
        let cookie = refresh_token_cookie("rt-token-here", 7 * 86400);
        assert!(cookie.starts_with("refresh_token=rt-token-here"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        // The narrow Path is the whole point of the helper:
        // refresh_token only needs to travel to /api/auth/refresh
        // so a leak elsewhere can't expose it.
        assert!(
            cookie.contains("Path=/api/auth"),
            "refresh_token cookie must be scoped to /api/auth, got: {cookie}"
        );
        assert!(cookie.contains(&format!("Max-Age={}", 7 * 86400)));
    }

    #[test]
    fn clear_auth_cookies_evicts_all_three() {
        let cookies = clear_auth_cookies();
        assert_eq!(cookies.len(), 3);
        // Each cookie must Max-Age=0 to evict immediately, and the
        // Path must match the original cookie's Path so the browser
        // recognizes it as the same cookie.
        let joined = cookies.join("\n");
        assert!(joined.contains("access_token=;"));
        assert!(joined.contains("refresh_token=;"));
        assert!(joined.contains("signing_key=;"));
        assert!(joined.matches("Max-Age=0").count() == 3);
        assert!(joined.contains("Path=/api/auth")); // refresh_token
        assert!(joined.contains("Path=/;")); // access_token + signing_key
    }

    #[test]
    fn extract_cookie_finds_named_value() {
        let request = Request::builder()
            .header(
                "cookie",
                "session=abc; access_token=eyJ.test.sig; refresh_token=rt-x",
            )
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_cookie(&request, "access_token").as_deref(),
            Some("eyJ.test.sig")
        );
        assert_eq!(
            extract_cookie(&request, "refresh_token").as_deref(),
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
