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

/// Slot inserted into request extensions by the access log layer so the
/// auth middleware can publish the authenticated user_id back to us after
/// it has verified the JWT. We can't read request extensions after
/// `inner.call(request)` consumes the request, so we share an `Arc<OnceLock>`
/// instead.
#[derive(Clone, Default)]
pub struct AccessLogUserSlot(pub Arc<OnceLock<Uuid>>);

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
        let future = self.inner.call(request);

        Box::pin(async move {
            let response = future.await?;
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
            if let Some(uid) = user_slot.0.get().copied() {
                entry = entry.user_id(uid);
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
