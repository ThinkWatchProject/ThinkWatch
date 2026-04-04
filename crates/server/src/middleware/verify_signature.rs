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
/// Returns the hex-encoded key.
pub async fn create_signing_key(
    redis: &fred::clients::Client,
    user_id: &uuid::Uuid,
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

    Ok(hex_key)
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
    // Skip CORS preflight
    if *request.method() == Method::OPTIONS {
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

    // Get signing key from Redis
    let signing_key_hex: Option<String> =
        fred::interfaces::KeysInterface::get(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(None);

    let signing_key_hex = signing_key_hex.ok_or_else(|| {
        tracing::warn!("No signing key found for user {user_id}");
        StatusCode::UNAUTHORIZED
    })?;
    let signing_key = hex::decode(&signing_key_hex).map_err(|e| {
        tracing::error!("Failed to hex-decode signing key for user {user_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

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
