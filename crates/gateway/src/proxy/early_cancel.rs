//! A client that leaves before it gets a response still leaves a row.
//!
//! Once a stream has started, a disconnect is recorded by the stream's
//! tail (`StreamOutcome::ClientCancelled`). Before that — while the key's
//! roles and limits load, the pre-flight stages run, a route is picked,
//! or a whole (non-streamed) answer is awaited — the request is just a
//! future, and when the client goes hyper drops it: nothing after the
//! await point runs, and there was no trace of the request at all.
//!
//! [`EarlyCancel`] is armed by the API-key middleware as soon as the key
//! is known, and disarmed when the handler hands back a response —
//! whatever it is, since every response path writes its own row. If it
//! is dropped still armed, the future was dropped: it writes one
//! `gateway_logs` row with status 499 and no tokens, no cost, no upstream.
//! The handler fills in what it learns on the way ([`EarlyCancelSlot`]):
//! the trace id and the model.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rust_decimal::Decimal;
use think_watch_common::audit::AuditLogger;

use super::GatewayRequestIdentity;
use super::body_capture::BodyCapture;
use super::log_ctx::emit_gateway_log_with_extra;

/// What is known about the request so far.
#[derive(Default)]
struct Known {
    identity: GatewayRequestIdentity,
    trace_id: Option<String>,
    session_id: Option<String>,
    model: Option<String>,
}

/// The handler's handle on the armed guard, carried as a request
/// extension.
#[derive(Clone)]
pub struct EarlyCancelSlot(Arc<Mutex<Known>>);

impl EarlyCancelSlot {
    /// The ids the request's other rows carry, once the handler has them.
    pub(crate) fn request(&self, trace_id: &str, session_id: Option<&str>) {
        if let Ok(mut k) = self.0.lock() {
            k.trace_id = Some(trace_id.to_string());
            k.session_id = session_id.map(str::to_string);
        }
    }

    /// The model the caller named, after aliasing.
    pub(crate) fn model(&self, model: &str) {
        if let Ok(mut k) = self.0.lock() {
            k.model = Some(model.to_string());
        }
    }
}

/// Writes the cancelled row if dropped before [`EarlyCancel::disarm`].
pub struct EarlyCancel {
    audit: AuditLogger,
    known: EarlyCancelSlot,
    started: Instant,
    armed: bool,
}

impl EarlyCancel {
    /// Arm for a request that arrived at `started`, from `identity` (what
    /// the middleware has resolved so far).
    pub fn arm(audit: AuditLogger, identity: GatewayRequestIdentity, started: Instant) -> Self {
        Self {
            audit,
            known: EarlyCancelSlot(Arc::new(Mutex::new(Known {
                identity,
                ..Default::default()
            }))),
            started,
            armed: true,
        }
    }

    /// The fuller identity, once the middleware has it.
    pub fn identity(&self, identity: &GatewayRequestIdentity) {
        if let Ok(mut k) = self.known.0.lock() {
            k.identity = identity.clone();
        }
    }

    pub fn slot(&self) -> EarlyCancelSlot {
        self.known.clone()
    }

    /// A response exists; it records itself.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for EarlyCancel {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(k) = self.known.0.lock() else {
            return;
        };
        metrics::counter!("gateway_cancelled_before_response_total").increment(1);
        let trace_id = k
            .trace_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let id = &k.identity;
        emit_gateway_log_with_extra(
            &self.audit,
            &trace_id,
            k.session_id.as_deref(),
            id.user_id.as_deref(),
            id.user_email.as_deref(),
            id.api_key_id.as_deref(),
            id.api_key_lineage_id.as_deref(),
            id.ip_address.as_deref(),
            k.model.as_deref().unwrap_or("(unknown)"),
            None,
            None,
            0,
            0,
            Decimal::ZERO,
            self.started.elapsed().as_millis() as i64,
            499,
            Some(serde_json::json!({
                // The marker a cancelled stream carries too.
                "stream_outcome": "client_cancelled",
                "cancelled_before": "response",
            })),
            BodyCapture::default(),
        );
    }
}
