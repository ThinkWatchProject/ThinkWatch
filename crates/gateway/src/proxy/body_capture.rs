//! Full request/response payload snapshots for the enterprise audit
//! trail. Gating + truncation + optional PII redaction happens once
//! per request inside [`prepare_body_capture`]; the resulting struct is
//! passed verbatim into every `emit_gateway_log*` call site so the
//! success / streaming / error / cache-hit paths all carry the same
//! payload semantics.
//!
//! Not shared with the desktop gateway, on purpose. That one hands
//! bodies to a local store through a small bounded channel and keeps
//! the first 256 KB of a response; this one is an audit trail — gated
//! per field by dynamic config, PII-redacted on request, offloaded to
//! object storage when oversize. The two answer different questions, and
//! one abstraction over both would serve neither.
//!
//! Body capture status values come from the shared
//! `think_watch_common::audit::BodyCaptureStatus` enum so the producer
//! side (this file + mcp-gateway) and consumer side (handlers, flush
//! mappers, tests, frontend) all agree on the wire spelling.

use std::sync::Arc;

use crate::pii_redactor::PiiRedactor;
use think_watch_common::audit::BodyCaptureStatus;
use think_watch_common::dynamic_config::DynamicConfig;

/// Captured payload snapshot. Cheap to construct + clone — the
/// strings are already truncated / redacted / serialized by
/// [`prepare_body_capture`] so the per-call-site cost is just two
/// `Option<String>` clones.
///
/// `request_bytes` / `response_bytes` carry the ORIGINAL body size,
/// not the cell size — when a body offloads to S3 the cell holds a
/// short `s3://...` URL while the byte count needs to reflect the
/// user's actual payload (auditors aggregating "total prompt bytes
/// captured this month" would otherwise see ~80 bytes per offloaded
/// row and a wildly wrong number). The flush mapper reads these in
/// preference to `cell.len()` exactly to break that asymmetry.
#[derive(Default, Clone)]
pub(crate) struct BodyCapture {
    pub(super) request: Option<String>,
    pub(super) response: Option<String>,
    pub(super) request_bytes: Option<u32>,
    pub(super) response_bytes: Option<u32>,
    pub(super) status: Option<&'static str>,
}

impl BodyCapture {
    pub(super) fn disabled() -> Self {
        Self {
            request: None,
            response: None,
            request_bytes: None,
            response_bytes: None,
            status: Some(BodyCaptureStatus::Disabled.as_str()),
        }
    }

    /// Attach the captured strings + status to an `AuditEntry`. No-op
    /// when all three fields are empty.
    pub(super) fn apply(
        self,
        mut entry: think_watch_common::audit::AuditEntry,
    ) -> think_watch_common::audit::AuditEntry {
        if let Some(r) = self.request {
            entry = entry.request_body(r);
        }
        if let Some(r) = self.response {
            entry = entry.response_body(r);
        }
        if let Some(b) = self.request_bytes {
            entry = entry.request_body_bytes(b);
        }
        if let Some(b) = self.response_bytes {
            entry = entry.response_body_bytes(b);
        }
        if let Some(s) = self.status {
            entry = entry.body_capture_status(s);
        }
        entry
    }
}

/// Walk a request body + optional response through the
/// dynamic-config-driven capture pipeline:
///   1. capture-enabled gate (per-field)
///   2. optional PII redaction (when `audit.body_redact_pii` is on)
///   3. blob-store offload when oversize and a backend is configured
///   4. byte-cap truncation when offload isn't available (fallback)
///
/// `messages` is the post-PII-redaction set the gateway already
/// passes to upstream; for the audit blob we want the version users
/// actually authored. The caller hands us the original
/// pre-redaction slice when both forms exist (`prepare_body_capture`
/// itself does not know which was sent upstream).
///
/// `trace_id` is woven into the offload object key so an operator
/// browsing the bucket can correlate objects back to the audit row
/// without a CH query.
pub(crate) async fn prepare_body_capture(
    dynamic_config: &DynamicConfig,
    pii_redactor: &PiiRedactor,
    blob_store: &Arc<dyn think_watch_common::blob_store::BlobStore>,
    trace_id: &str,
    request: &[u8],
    response: Option<&[u8]>,
) -> BodyCapture {
    let capture_req = dynamic_config.audit_capture_request_bodies().await;
    let capture_resp = dynamic_config.audit_capture_response_bodies().await;
    if !capture_req && !capture_resp {
        return BodyCapture::disabled();
    }
    let max_bytes = dynamic_config.audit_body_max_bytes().await as usize;
    let redact_pii = dynamic_config.audit_body_redact_pii().await;
    let can_offload = blob_store.can_offload();

    let mut truncated_flag = false;
    let mut offloaded_flag = false;
    let mut request_bytes: Option<u32> = None;
    let mut response_bytes: Option<u32> = None;
    let request = if capture_req {
        let raw = String::from_utf8_lossy(request).into_owned();
        request_bytes = Some(raw.len() as u32);
        Some(
            process_body(
                raw,
                max_bytes,
                redact_pii,
                pii_redactor,
                blob_store,
                can_offload,
                trace_id,
                "request",
                &mut truncated_flag,
                &mut offloaded_flag,
            )
            .await,
        )
    } else {
        None
    };
    let response_body = match (capture_resp, response) {
        (true, Some(resp)) => {
            let raw = String::from_utf8_lossy(resp).into_owned();
            response_bytes = Some(raw.len() as u32);
            Some(
                process_body(
                    raw,
                    max_bytes,
                    redact_pii,
                    pii_redactor,
                    blob_store,
                    can_offload,
                    trace_id,
                    "response",
                    &mut truncated_flag,
                    &mut offloaded_flag,
                )
                .await,
            )
        }
        _ => None,
    };
    let status = if request.is_none() && response_body.is_none() {
        BodyCaptureStatus::Disabled.as_str()
    } else if offloaded_flag {
        // Offload + truncation are mutually exclusive per field, but a
        // single emit can carry one offloaded body and one truncated
        // body if e.g. request fit inline and response was huge. Pick
        // `offloaded` as the dominant status because it's the more
        // informative one — truncation would lose data, offload didn't.
        BodyCaptureStatus::Offloaded.as_str()
    } else if truncated_flag {
        BodyCaptureStatus::Truncated.as_str()
    } else {
        BodyCaptureStatus::Captured.as_str()
    };
    BodyCapture {
        request,
        response: response_body,
        request_bytes,
        response_bytes,
        status: Some(status),
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_body(
    mut s: String,
    max_bytes: usize,
    redact_pii: bool,
    pii_redactor: &PiiRedactor,
    blob_store: &Arc<dyn think_watch_common::blob_store::BlobStore>,
    can_offload: bool,
    trace_id: &str,
    field: &'static str,
    truncated_flag: &mut bool,
    offloaded_flag: &mut bool,
) -> String {
    if redact_pii {
        s = pii_redactor.redact_blob(&s);
    }
    if s.len() <= max_bytes {
        return s;
    }
    // Oversize. If a blob store is wired in, offload — auditors keep
    // the full payload at the cost of one S3 round-trip. If not, fall
    // back to char-boundary-safe truncation so we still record SOMETHING
    // (a `truncated` row beats a NULL one for investigations).
    if can_offload {
        use think_watch_common::blob_store::{BlobDecision, BlobKeyHint};
        let hint = BlobKeyHint {
            table: "gateway_logs",
            log_id: trace_id,
            field,
        };
        match blob_store
            .store_if_oversize(hint, std::mem::take(&mut s), max_bytes)
            .await
        {
            Ok(BlobDecision::Offloaded { url, .. }) => {
                *offloaded_flag = true;
                return url;
            }
            Ok(BlobDecision::Inline(returned)) => {
                // Shouldn't happen — store_if_oversize was called with
                // a string already > max_bytes — but if a future store
                // impl changes its mind, fall through to truncation.
                s = returned;
            }
            Err(e) => {
                tracing::warn!(
                    field,
                    trace_id,
                    error = %e,
                    "blob offload failed; falling back to truncation"
                );
                metrics::counter!(
                    "audit_body_offload_failed_total",
                    "field" => field.to_string()
                )
                .increment(1);
                s = format!("[blob offload failed: {e}]");
            }
        }
    }
    *truncated_flag = true;
    metrics::counter!(
        "audit_body_truncated_total",
        "field" => field.to_string(),
    )
    .increment(1);
    // Leave room for the ellipsis sentinel and walk back to the
    // nearest UTF-8 char boundary — provider names / model tokens /
    // user prompts routinely include non-ASCII (CJK, emoji), and a
    // naive byte slice would panic mid-codepoint.
    let budget = max_bytes.saturating_sub(3);
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}
