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
use think_watch_common::audit::{AuditEntry, AuditLogger, LogType};
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

/// Per-request correlation id inserted into request extensions so any
/// downstream audit emission can tag itself with it. Mirrors what the
/// gateway puts in `metadata.request_id` for AI traffic — setting this
/// from the access_log layer means management endpoints get a trace id
/// too, and a single `GET /api/admin/trace/{id}` query returns every
/// row from audit_logs, gateway_logs, or mcp_logs that shares it.
///
/// Clients can pin the id by sending the `x-trace-id` header; otherwise
/// a fresh UUID is minted. We echo the chosen id back on the response
/// as `x-trace-id` so operators can copy it out of a cURL trace.
#[derive(Clone, Debug)]
#[allow(dead_code)] // read by handlers that tag audit entries with .trace_id()
pub struct RequestTraceId(pub String);

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
        request
            .extensions_mut()
            .insert(RequestTraceId(trace_id.clone()));

        // Slot for auth_guard to publish the resolved user_id into.
        let user_slot = AccessLogUserSlot::default();
        request.extensions_mut().insert(user_slot.clone());
        let user_agent = request
            .headers()
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
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

            // Resolve client IP using the same logic as auth_guard
            let ip = match dc.client_ip_source().await.as_str() {
                "xff" => {
                    let position = dc.client_ip_xff_position().await;
                    let depth = dc.client_ip_xff_depth().await.max(1) as usize;
                    headers
                        .get("x-forwarded-for")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| {
                            let parts: Vec<&str> = v.split(',').map(|s| s.trim()).collect();
                            let idx = if position == "right" {
                                parts.len().checked_sub(depth)
                            } else {
                                let i = depth - 1;
                                if i < parts.len() { Some(i) } else { None }
                            };
                            idx.and_then(|i| parts.get(i)).map(|s| s.to_string())
                        })
                        .or(connection_ip)
                }
                "x-real-ip" => headers
                    .get("x-real-ip")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.trim().to_string())
                    .or(connection_ip),
                _ => connection_ip,
            };

            let mut entry = AuditEntry::new("http.request")
                .log_type(LogType::Access)
                .detail(serde_json::json!({
                    "method": method,
                    "path": path,
                    "status_code": status_code,
                    "latency_ms": latency_ms,
                    "port": port,
                }));
            if let Some(info) = user_slot.0.get() {
                entry = entry.user_id(info.user_id);
                if let Some(ref email) = info.user_email {
                    entry = entry.user_email(email);
                }
            }
            if let Some(ip) = ip {
                entry = entry.ip_address(ip);
            }
            if let Some(ua) = user_agent {
                entry = entry.user_agent(ua);
            }
            audit.log(entry);

            Ok(response)
        })
    }
}
