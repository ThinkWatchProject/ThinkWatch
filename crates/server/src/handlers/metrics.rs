use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::PrometheusHandle;

/// State for the `/metrics` route — the recorder handle and the
/// bearer token that's required on every request. The route is
/// only mounted when `METRICS_BEARER_TOKEN` is set in env (see
/// `app.rs`), so by the time we get here the token is guaranteed
/// to be present and non-empty.
#[derive(Clone)]
pub struct MetricsState {
    pub handle: PrometheusHandle,
    pub bearer_token: String,
}

/// GET /metrics — Prometheus metrics endpoint.
///
/// Lives on the public gateway port (3000) so Prometheus scrapers
/// can pull from network-accessible deployments. The route is
/// only mounted when `METRICS_BEARER_TOKEN` is set; if you're
/// getting 404 here, set the env var and restart. Scrapers must
/// pass `Authorization: Bearer <value>`.
pub async fn prometheus_metrics(State(state): State<MetricsState>, headers: HeaderMap) -> Response {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    // Constant-time compare to avoid timing oracle on the token.
    let ok = !presented.is_empty()
        && subtle::ConstantTimeEq::ct_eq(presented.as_bytes(), state.bearer_token.as_bytes())
            .unwrap_u8()
            == 1;
    if !ok {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let body = state.handle.render();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// Install the Prometheus recorder and return the handle for rendering.
///
/// Returns an error if the recorder could not be installed — typically
/// this is the second call on the same process (the global recorder
/// is already registered) or a platform-level issue. The caller is
/// expected to propagate the error so startup fails loudly rather than
/// silently dropping metrics: without a recorder registered, every
/// `counter!`/`histogram!` macro becomes a no-op and dashboards flat-
/// line with no clue why.
pub fn install_prometheus_recorder() -> anyhow::Result<PrometheusHandle> {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to install Prometheus recorder ({e}). \
                 If this is not the first call, the global recorder is already \
                 registered for this process — only one /metrics endpoint should \
                 exist per binary."
            )
        })
}
