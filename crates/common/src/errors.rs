use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Default Retry-After (seconds) advertised on 429 / 503 responses
/// when the caller didn't provide a more specific value. Picked at
/// 60s to bias clients toward longer backoff than the typical 1-5s
/// exponential ramp — these errors usually come from upstream
/// quota windows or transient infra outages that don't recover in
/// under a minute.
const DEFAULT_RETRY_AFTER_SECS: u32 = 60;

/// Application-facing error.
///
/// Each named variant maps to a specific HTTP status and a fixed
/// `type` tag the frontend can switch on; the generic `Internal`
/// variant is the catch-all for unexpected failures and is logged
/// server-side with full context while the client sees only a
/// sanitised message.
///
/// When you reach for `Internal(anyhow::anyhow!("..."))` because of
/// a DB/Redis/upstream hiccup that the user can retry past, prefer
/// `ServiceUnavailable` instead — the caller gets a 503 (retryable)
/// rather than a 500 (treat as a bug) and log volume for genuinely
/// unexpected Internal cases stays clean.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Authentication required")]
    Unauthorized,

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("Rate limit exceeded")]
    RateLimited,

    #[error("{0}")]
    Conflict(String),

    /// Transient dependency failure — DB outage, Redis unreachable,
    /// upstream provider refusing connections, etc. Maps to HTTP 503
    /// so clients can retry with backoff; the message is operator-
    /// facing and omitted from the response body.
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    message: String,
    r#type: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_type, public_message) = match &self {
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication required".to_string(),
            ),
            AppError::Forbidden(reason) => (StatusCode::FORBIDDEN, "forbidden", reason.clone()),
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m.clone()),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Rate limit exceeded".to_string(),
            ),
            AppError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            AppError::ServiceUnavailable(m) => {
                // Log operator-facing reason server-side; clients get a
                // generic retryable message so we don't leak infra
                // details through the error shape.
                tracing::warn!("Service unavailable: {m}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service_unavailable",
                    "Service temporarily unavailable — please retry".to_string(),
                )
            }
            AppError::Internal(e) => {
                // Log the full chain on the server (operator visibility)
                // but NEVER return it to the client. Internal errors can
                // contain DB query params, parsed user input, file paths,
                // or other details that aid an attacker. The client only
                // gets a generic "internal error" string.
                tracing::error!("Internal server error: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error".to_string(),
                )
            }
        };

        let body = ErrorResponse {
            error: ErrorBody {
                message: public_message,
                r#type: error_type.to_string(),
            },
        };

        let mut response = (status, axum::Json(body)).into_response();
        // RFC 6585 §4 (429) and RFC 7231 §6.6.4 (503) both recommend a
        // Retry-After hint so clients can back off appropriately
        // instead of guessing. Without this, well-behaved SDKs default
        // to a tight 1-5s exponential ramp that hammers the same
        // quota window the rate limiter is trying to drain.
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(DEFAULT_RETRY_AFTER_SECS),
            );
        }
        response
    }
}

impl From<tw_crypto::json_secret::SecretError> for AppError {
    /// The shared layer must not know this crate's error taxonomy — that
    /// is why `tw-crypto` carries its own `SecretError` rather than
    /// returning `AppError` directly (thinkwatch-core DESIGN §9.3).
    ///
    /// A secret that will not decrypt is always `Internal`: the caller
    /// supplied a well-formed request, and the failure is either a wrong
    /// key or a corrupted ciphertext — both of which are ours to fix, not
    /// theirs. Mapping it to `BadRequest` would tell a user to change
    /// something they never controlled.
    fn from(err: tw_crypto::json_secret::SecretError) -> Self {
        AppError::Internal(anyhow::anyhow!("{err}"))
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        // A genuine schema bug is still `Internal`; transient
        // connectivity is `ServiceUnavailable`. Unique-violation
        // bubbles up as `Conflict` so concurrent inserts that race
        // past a SELECT-EXISTS check don't 500 the caller — the
        // intent ("this key already exists") survives end-to-end.
        // Distinguishing all three lets dashboards separate
        // "Postgres is down" from "a query is broken" from "a
        // benign concurrent-write conflict."
        match &err {
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
            | sqlx::Error::Io(_) => {
                tracing::error!("Database unavailable: {err:?}");
                AppError::ServiceUnavailable("Database unavailable".into())
            }
            sqlx::Error::Database(db_err)
                if matches!(db_err.kind(), sqlx::error::ErrorKind::UniqueViolation) =>
            {
                // The PG SQLSTATE for unique_violation is `23505`.
                // We surface this as 409 so register / setup / admin
                // create / SSO provisioning all return a clean
                // Conflict on case-only racy duplicates against the
                // `LOWER(email)` functional index without each
                // handler needing its own ErrorKind sniffing.
                tracing::debug!("Database unique violation: {err:?}");
                AppError::Conflict("Resource already exists".into())
            }
            _ => {
                tracing::error!("Database error: {err:?}");
                AppError::Internal(anyhow::anyhow!("Database error"))
            }
        }
    }
}

impl From<fred::error::Error> for AppError {
    fn from(err: fred::error::Error) -> Self {
        // Redis failures are almost always transient — surface them
        // as 503 so clients retry instead of reporting "bug".
        tracing::error!("Redis error: {err:?}");
        AppError::ServiceUnavailable("Cache unavailable".into())
    }
}
