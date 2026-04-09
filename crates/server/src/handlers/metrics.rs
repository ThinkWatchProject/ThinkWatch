use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::PrometheusHandle;

/// State for the `/metrics` route — the recorder handle and an
/// optional bearer token. When the token is `Some`, requests must
/// present it in the `Authorization: Bearer <token>` header. When
/// `None`, the endpoint stays open (legacy behavior, useful in
/// dev).
#[derive(Clone)]
pub struct MetricsState {
    pub handle: PrometheusHandle,
    pub bearer_token: Option<String>,
}

/// GET /metrics — Prometheus metrics endpoint.
///
/// Lives on the public gateway port (3000) so Prometheus scrapers
/// can pull from network-accessible deployments. Without a token
/// the gateway leaks cost / token-usage / error-rate signals to
/// anyone who can reach the port. Set `METRICS_BEARER_TOKEN` to a
/// random secret and configure the scrape job with a matching
/// `bearer_token` (or `Authorization` header).
pub async fn prometheus_metrics(State(state): State<MetricsState>, headers: HeaderMap) -> Response {
    if let Some(ref expected) = state.bearer_token {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        // Constant-time compare to avoid timing oracle on the token.
        let ok = subtle::ConstantTimeEq::ct_eq(presented.as_bytes(), expected.as_bytes())
            .unwrap_u8()
            == 1
            && !presented.is_empty();
        if !ok {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    let body = state.handle.render();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// Install the Prometheus recorder and return the handle for rendering.
pub fn install_prometheus_recorder() -> PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder")
}
