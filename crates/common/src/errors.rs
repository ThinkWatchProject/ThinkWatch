use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Authentication required")]
    Unauthorized,

    #[error("Insufficient permissions")]
    Forbidden,

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("Rate limit exceeded")]
    RateLimited,

    #[error("{0}")]
    Conflict(String),

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
        let (status, error_type) = match &self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            AppError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            AppError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            AppError::Internal(e) => {
                tracing::error!("Internal server error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };

        let body = ErrorResponse {
            error: ErrorBody {
                message: self.to_string(),
                r#type: error_type.to_string(),
            },
        };

        (status, axum::Json(body)).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!("Database error: {err:?}");
        AppError::Internal(anyhow::anyhow!("Database error"))
    }
}

impl From<fred::error::Error> for AppError {
    fn from(err: fred::error::Error) -> Self {
        tracing::error!("Redis error: {err:?}");
        AppError::Internal(anyhow::anyhow!("Cache error"))
    }
}
