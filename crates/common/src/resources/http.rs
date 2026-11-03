//! Body admission precedes extraction; checking in the handler is too late.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use super::{ByteBudget, policy};

/// Preserve Axum's JSON errors while moving large/concurrent decoding off I/O.
pub struct ScheduledJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for ScheduledJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let json = request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split(';').next())
            .is_some_and(|h| {
                let h = h.trim().to_ascii_lowercase();
                h == "application/json" || (h.starts_with("application/") && h.ends_with("+json"))
            });
        if !json {
            return Err((
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Expected request with Content-Type: application/json",
            )
                .into_response());
        }
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(IntoResponse::into_response)?;
        super::cpu(bytes.len(), || axum::Json::<T>::from_bytes(&bytes))
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(IntoResponse::into_response)
    }
}

pub async fn admit(request: Request, next: Next) -> Response {
    admit_with_budget(request, next, super::body_budget(), policy().max_body_bytes).await
}

async fn admit_with_budget(
    request: Request,
    next: Next,
    budget: &Arc<ByteBudget>,
    max: usize,
) -> Response {
    let (parts, body) = request.into_parts();
    if body.size_hint().upper().is_some_and(|n| n > max as u64) {
        return rejection(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
    let mut data = Vec::new();
    let mut lease = budget.reserve();
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                use std::error::Error;
                let status = if error
                    .source()
                    .is_some_and(|e| e.is::<http_body_util::LengthLimitError>())
                {
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    axum::http::StatusCode::BAD_REQUEST
                };
                return rejection(status);
            }
        };
        let size = data.len().saturating_add(chunk.len());
        if size > max {
            return rejection(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        }
        if !lease.grow(size.saturating_mul(super::BODY_HEADROOM)) {
            return rejection(axum::http::StatusCode::TOO_MANY_REQUESTS);
        }
        data.extend_from_slice(&chunk);
    }
    let data = lease.wrap(data.into());
    let mut response = next
        .run(Request::from_parts(parts, Body::from(data.clone())))
        .await;
    let body = std::mem::replace(response.body_mut(), Body::empty());
    *response.body_mut() = Body::new(HeldBody {
        inner: body,
        _request: data,
    });
    response
}

fn rejection(status: axum::http::StatusCode) -> Response {
    let busy = status == axum::http::StatusCode::TOO_MANY_REQUESTS;
    let message = if busy {
        "The gateway is buffering other requests. Retry shortly."
    } else if status == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        "The request body exceeds the gateway buffering limit."
    } else {
        "The request body could not be read."
    };
    let mut response = (status, axum::Json(serde_json::json!({
        "error": {"type": if busy { "rate_limit_error" } else { "invalid_request_error" }, "message": message}
    }))).into_response();
    if busy {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, "1".parse().unwrap());
        metrics::counter!("gateway_memory_busy_total").increment(1);
    }
    response
}

/// A JSON extractor discards the input Bytes while its decoded object still
/// exists. Retain admission through the response, including streaming and drop.
struct HeldBody {
    inner: Body,
    _request: Bytes,
}

impl HttpBody for HeldBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use tower::ServiceExt;

    fn router(budget: Arc<ByteBudget>, max: usize) -> Router {
        Router::new()
            .route("/", post(|body: Bytes| async move { body }))
            .layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let budget = budget.clone();
                    async move { admit_with_budget(request, next, &budget, max).await }
                },
            ))
    }

    #[tokio::test]
    async fn chunked_pressure_returns_retryable_429_and_frees_partial_buffers() {
        let budget = ByteBudget::new(25 * super::super::BODY_HEADROOM);
        let app = router(budget.clone(), 64);
        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 20])),
            Ok(Bytes::from(vec![b'b'; 10])),
        ]);
        let request = Request::post("/").body(Body::from_stream(chunks)).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["retry-after"], "1");
        assert_eq!(budget.used(), 0);

        let request = Request::post("/").body(Body::from(vec![b'x'; 16])).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            budget.used(),
            16 * super::super::BODY_HEADROOM,
            "input admission outlives the extractor"
        );
        let body = axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap();
        assert_eq!(body.len(), 16);
        drop(body);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn an_oversize_body_is_413_and_cancellation_releases_partial_admission() {
        let budget = ByteBudget::new(25 * super::super::BODY_HEADROOM);
        let app = router(budget.clone(), 20);
        let response = app
            .clone()
            .oneshot(Request::post("/").body(Body::from(vec![0; 21])).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert_eq!(budget.used(), 0);

        let sent = Arc::new(tokio::sync::Notify::new());
        let started = sent.clone();
        let chunks = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(b"12345678"));
            started.notify_one();
            std::future::pending::<()>().await;
        };
        let task =
            tokio::spawn(app.oneshot(Request::post("/").body(Body::from_stream(chunks)).unwrap()));
        sent.notified().await;
        assert_eq!(budget.used(), 8 * super::super::BODY_HEADROOM);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(budget.used(), 0);
    }
}
