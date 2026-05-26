use axum::extract::Request;
use axum::response::Response;
use std::{
    future::Future,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};
use tower::{Layer, Service};
use uuid::Uuid;

use std::sync::Arc;
use think_watch_common::audit::{AuditActor, AuditLogger, LogType};
use think_watch_common::dynamic_config::DynamicConfig;

/// Identity published by the auth middleware into the access log's
/// request-scoped slot. Both the UUID and a snapshot of the user's
/// email get captured so access_logs can record `user_email` without a
/// separate PG lookup in the logging hot path.
#[derive(Clone, Debug)]
pub struct AccessLogUserInfo {
    pub user_id: Uuid,
    pub user_email: Option<String>,
}

/// Slot inserted into request extensions by the access log layer so the
/// auth middleware can publish the authenticated user back to us after
/// it has verified the JWT. We can't read request extensions after
/// `inner.call(request)` consumes the request, so we share an `Arc<OnceLock>`
/// instead.
#[derive(Clone, Default)]
pub struct AccessLogUserSlot(pub Arc<OnceLock<AccessLogUserInfo>>);

// Note: this middleware previously inserted a `RequestTraceId(String)`
// type into `request.extensions_mut()` with a comment claiming
// downstream handlers read it back to tag audit rows with `.trace_id()`.
// A workspace-wide grep showed zero such readers — gateway / MCP audit
// rows derive their trace id from the lifecycle state's
// `metadata.request_id` instead, and admin handlers don't tag at all.
// The struct + insert were removed in commit (this one) to stop
// pretending the wire-up exists. If a future audit row needs the
// access-layer trace id, add the extractor at the call site AND a
// reader test before re-introducing the extension entry.

/// Should the path/status pair be excluded from access_logs entirely?
///
/// Background: the dashboard self-poll alone generates ~280 access_log
/// rows/hour just from being open in a browser tab, and the log query
/// page becomes unreadable when 99% of rows are infra noise. Three
/// classes are filtered:
///
/// * `/api/health` (and any sub-path) — pure infra probe, no audit
///   value, hits ~55×/h from container orchestrators alone.
/// * `101 Switching Protocols` — WebSocket / HTTP upgrade handshakes,
///   not business requests; the WS session itself isn't auditable
///   through this layer anyway.
/// * Dashboard/analytics read-only stats endpoints — the live
///   observability surface the dashboard polls. These are GETs over
///   already-aggregated rollups, carry no user-supplied data beyond
///   `range`, and are not security-relevant. With three of them
///   running on a ~45s cadence per open tab, they dominate access_logs
///   on any deployment that has the dashboard open.
///
/// Kept intentionally narrow — every other endpoint, including auth
/// failures and other infra calls, still flows through so we don't
/// quietly drop attacker recon or buggy clients. Only GET requests on
/// the matching paths are filtered; if any of these ever gains a POST
/// variant it stays auditable.
pub fn is_access_log_noise(method: &str, path: &str, status_code: u16) -> bool {
    if status_code == 101 {
        return true;
    }
    if path == "/api/health" || path.starts_with("/api/health/") {
        return true;
    }
    if method == "GET"
        && matches!(
            path,
            "/api/dashboard/stats"
                | "/api/dashboard/live"
                | "/api/dashboard/ws-ticket"
                | "/api/dashboard/layout"
                | "/api/analytics/usage/stats"
                | "/api/analytics/costs/stats"
        )
    {
        return true;
    }
    false
}

/// Layer that logs HTTP requests to ClickHouse.
#[derive(Clone)]
pub struct AccessLogLayer {
    audit: AuditLogger,
    dynamic_config: Arc<DynamicConfig>,
    port: u16,
}

impl AccessLogLayer {
    pub fn new(audit: AuditLogger, dynamic_config: Arc<DynamicConfig>, port: u16) -> Self {
        Self {
            audit,
            dynamic_config,
            port,
        }
    }
}

impl<S> Layer<S> for AccessLogLayer {
    type Service = AccessLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessLogService {
            inner,
            audit: self.audit.clone(),
            dynamic_config: self.dynamic_config.clone(),
            port: self.port,
        }
    }
}

#[derive(Clone)]
pub struct AccessLogService<S> {
    inner: S,
    audit: AuditLogger,
    dynamic_config: Arc<DynamicConfig>,
    port: u16,
}

impl<S> Service<Request> for AccessLogService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request) -> Self::Future {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();

        // Resolve or mint the trace id for this request. Headers are
        // validated to be sensible ASCII (<= 128 chars, no control
        // characters) — anything else we ignore and generate fresh.
        let incoming_trace = request
            .headers()
            .get("x-trace-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()));
        let trace_id = incoming_trace.unwrap_or_else(|| Uuid::new_v4().to_string());

        // Slot for auth_guard to publish the resolved user_id into.
        let user_slot = AccessLogUserSlot::default();
        request.extensions_mut().insert(user_slot.clone());
        let user_agent = request
            .headers()
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let connection_ip = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());

        let audit = self.audit.clone();
        let dc = self.dynamic_config.clone();
        let port = self.port;
        let headers = request.headers().clone();
        let start = std::time::Instant::now();
        let trace_for_response = trace_id.clone();
        let future = self.inner.call(request);

        Box::pin(async move {
            let mut response = future.await?;
            // Echo the trace id back to the client unless the downstream
            // handler already set one (which would be odd but we let it
            // win). Parsing failure (never happens for a UUID) silently
            // skips the header — the event is still tagged in CH.
            if !response.headers().contains_key("x-trace-id")
                && let Ok(v) = trace_for_response.parse()
            {
                response.headers_mut().insert("x-trace-id", v);
            }
            let latency_ms = start.elapsed().as_millis() as i64;
            let status_code = response.status().as_u16();

            // Skip pure infrastructure noise (health probes, protocol
            // upgrades, dashboard self-poll) before resolving IP /
            // building the entry — the skip filter dominates write
            // traffic on busy dashboards.
            if is_access_log_noise(&method, &path, status_code) {
                return Ok(response);
            }

            // Delegate to the shared resolver so the trusted-proxy
            // contract is enforced identically here and in auth_guard.
            // The previous inlined copy honored XFF / X-Real-IP
            // unconditionally, letting any direct connection spoof the
            // IP recorded in access_logs / IP-keyed analytics.
            let ip = crate::middleware::auth_guard::resolve_client_ip(&dc, &headers, connection_ip)
                .await;

            // Build the access-log entry through `AnonymousActor`:
            // even though some requests are authenticated, this
            // middleware doesn't have an `AuthUser` extractor in scope —
            // the per-request user_id slot is filled by the auth
            // middleware *after* this `before_response` closure was
            // captured. Use the actor for IP/UA/email and chain
            // user_id manually from the slot when present. `.log_type`
            // overrides the actor's default LogType::Audit since this
            // is the access-log table, not the audit-log table.
            let user_info = user_slot.0.get();
            let actor = think_watch_common::audit::AnonymousActor {
                ip: ip.as_deref(),
                user_agent: user_agent.as_deref(),
                user_email: user_info.and_then(|i| i.user_email.as_deref()),
                user_id: user_info.map(|i| i.user_id),
            };
            audit.log(
                actor
                    .audit("http.request")
                    .log_type(LogType::Access)
                    .detail(serde_json::json!({
                        "method": method,
                        "path": path,
                        "status_code": status_code,
                        "latency_ms": latency_ms,
                        "port": port,
                    })),
            );

            Ok(response)
        })
    }
}

#[cfg(test)]
mod noise_filter_tests {
    use super::is_access_log_noise;

    #[test]
    fn health_probe_paths_are_skipped() {
        assert!(is_access_log_noise("GET", "/api/health", 200));
        assert!(is_access_log_noise("GET", "/api/health/", 200));
        assert!(is_access_log_noise("GET", "/api/health/ready", 503));
    }

    #[test]
    fn websocket_upgrades_are_skipped_regardless_of_path() {
        // The 101 case is the one that drives operators batty — a real
        // status leaking through "status_code:200" filters because the
        // upgrade returns 101.
        assert!(is_access_log_noise("GET", "/api/dashboard/ws", 101));
        assert!(is_access_log_noise("GET", "/anything/at/all", 101));
    }

    #[test]
    fn dashboard_self_poll_endpoints_are_skipped_on_get() {
        // The three stat endpoints + their live/layout/ws-ticket
        // siblings dominate access_logs on any deployment with the
        // dashboard open — ~280 rows/hour per open tab.
        for path in [
            "/api/dashboard/stats",
            "/api/dashboard/live",
            "/api/dashboard/ws-ticket",
            "/api/dashboard/layout",
            "/api/analytics/usage/stats",
            "/api/analytics/costs/stats",
        ] {
            assert!(
                is_access_log_noise("GET", path, 200),
                "expected GET {path} to be filtered"
            );
        }
    }

    #[test]
    fn non_get_on_polling_paths_is_kept() {
        // Defensive: if any of these paths ever gains a POST/PATCH/etc
        // it should stay auditable. Only GETs are infra noise.
        assert!(!is_access_log_noise("POST", "/api/dashboard/stats", 200));
        assert!(!is_access_log_noise("DELETE", "/api/dashboard/layout", 200));
    }

    #[test]
    fn ordinary_traffic_is_kept() {
        assert!(!is_access_log_noise("GET", "/api/keys", 200));
        assert!(!is_access_log_noise("POST", "/api/keys", 201));
        assert!(!is_access_log_noise("GET", "/api/admin/access-logs", 401));
        // healthz lookalikes that aren't ours — don't accidentally
        // swallow a route a future handler adds.
        assert!(!is_access_log_noise("GET", "/healthz", 200));
        assert!(!is_access_log_noise(
            "GET",
            "/api/something/health-check",
            200
        ));
    }
}
