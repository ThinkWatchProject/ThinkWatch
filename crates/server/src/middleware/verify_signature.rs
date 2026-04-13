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
pub async fn store_public_key(
    redis: &fred::clients::Client,
    user_id: &uuid::Uuid,
    pubkey_jwk_json: &str,
    client_ip: Option<&str>,
) -> anyhow::Result<()> {
    let redis_key = format!("signing_pubkey:{user_id}");
    // Store with 24h TTL (matches refresh token lifetime roughly)
    fred::interfaces::KeysInterface::set::<(), _, _>(
        redis,
        &redis_key,
        pubkey_jwk_json,
        Some(fred::types::Expiration::EX(86400)),
        None,
        false,
    )
    .await?;

    // Store the IP the public key was registered from for session binding
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

    Ok(())
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
        "refresh_token={token}; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/refresh; Max-Age={max_age_secs}"
    )
}

/// Build the Set-Cookie values that clear the auth cookies.
/// Used by the logout handler to evict the session from the
/// browser without relying on the client to do anything.
pub fn clear_auth_cookies() -> [String; 3] {
    [
        "access_token=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0".to_string(),
        "refresh_token=; HttpOnly; Secure; SameSite=Lax; Path=/api/auth/refresh; Max-Age=0"
            .to_string(),
        // Also clear old broader-path cookie for upgrade path
        "refresh_token=; HttpOnly; Secure; SameSite=Lax; Path=/api/auth; Max-Age=0".to_string(),
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
    let user_id = request
        .extensions()
        .get::<super::auth_guard::AuthUser>()
        .map(|u| u.claims.sub)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // If no signature headers at all → the client hasn't registered a key pair yet
    // (race between register-key and the first request after login). Allow through —
    // the request is still authenticated via JWT cookie, just not signed.
    let has_sig_headers = request.headers().contains_key(HEADER_SIGNATURE);
    if !has_sig_headers {
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
    let was_set: bool = fred::interfaces::KeysInterface::set(
        &state.redis,
        &nonce_key,
        "1",
        Some(fred::types::Expiration::EX(nonce_ttl)),
        Some(fred::types::SetOptions::NX),
        false,
    )
    .await
    .unwrap_or(false);

    if !was_set {
        tracing::warn!("Duplicate nonce detected: {nonce}");
        return Err(StatusCode::UNAUTHORIZED);
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
    // public key was registered from. Fail-closed: if a session was issued
    // without a bound IP for some reason (legacy session, Redis flush
    // mid-rollout), reject so we don't silently accept session-replay
    // attacks.
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
        assert!(
            cookie.contains("Path=/api/auth/refresh"),
            "refresh_token cookie must be scoped to /api/auth/refresh, got: {cookie}"
        );
        assert!(cookie.contains(&format!("Max-Age={}", 7 * 86400)));
    }

    #[test]
    fn clear_auth_cookies_evicts_all() {
        let cookies = clear_auth_cookies();
        assert_eq!(cookies.len(), 3);
        let joined = cookies.join("\n");
        assert!(joined.contains("access_token=;"));
        assert!(joined.contains("refresh_token=;"));
        assert!(joined.matches("Max-Age=0").count() == 3);
        assert!(joined.contains("Path=/api/auth")); // refresh_token
        assert!(joined.contains("Path=/;")); // access_token
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
