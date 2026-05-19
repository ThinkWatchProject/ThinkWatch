//! Audit emission pipeline for MCP `tools/call`. Owns the body-
//! capture / redaction / offload helper plus the
//! `emit_tools_call_audit` method that both the buffered and
//! streaming code paths funnel through so `mcp_logs` rows are
//! shaped identically regardless of how the call terminated.

use super::McpProxy;
use super::jsonrpc::JsonRpcResponse;
use uuid::Uuid;

/// Body-capture truncation + offload + at-rest PII redaction for
/// `mcp_logs.tool_arguments` / `tool_result`. Mirrors the gateway-
/// side `process_body` semantics so both audit pipelines share the
/// same char-boundary-safe truncation + offload contract; redaction
/// flows through the shared `common::pii::BlobRedactor` so an
/// operator rule edit applies to both surfaces atomically.
#[allow(clippy::too_many_arguments)]
async fn apply_mcp_body_capture(
    mut s: String,
    max_bytes: usize,
    redact_pii: bool,
    blob_redactor: &think_watch_common::pii::BlobRedactor,
    blob_store: &std::sync::Arc<dyn think_watch_common::blob_store::BlobStore>,
    trace_id: &str,
    field: &'static str,
    truncated_flag: &mut bool,
    offloaded_flag: &mut bool,
) -> String {
    // Redact BEFORE the size check so the cap measures the version
    // that will actually land in audit storage — operators expect
    // `audit.body_max_bytes` to bound what's WRITTEN, not what was
    // sent.
    if redact_pii && !blob_redactor.is_empty() {
        s = blob_redactor.redact_blob(&s);
    }
    if s.len() <= max_bytes {
        return s;
    }
    if blob_store.can_offload() {
        use think_watch_common::blob_store::{BlobDecision, BlobKeyHint};
        let hint = BlobKeyHint {
            table: "mcp_logs",
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
                s = returned;
            }
            Err(e) => {
                tracing::warn!(
                    field,
                    trace_id,
                    error = %e,
                    "MCP blob offload failed; falling back to truncation"
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
    // Same operator-facing truncation signal as the gateway side.
    // Pair with `audit_body_offload_failed_total` in dashboards to
    // distinguish "S3 down" from "no S3 configured".
    metrics::counter!(
        "audit_body_truncated_total",
        "field" => field.to_string(),
    )
    .increment(1);
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

impl McpProxy {
    /// Emit the `mcp_logs` audit row for a completed tools/call.
    ///
    /// Centralised so the buffered and chunk-by-chunk streaming paths
    /// share a single, deterministic audit pipeline — the streaming
    /// path calls this once from its detached on_done task after the
    /// upstream stream completes (or the client disconnects),
    /// guaranteeing the row is emitted exactly once regardless of
    /// how the call terminated.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn emit_tools_call_audit(
        &self,
        user_id: Uuid,
        user_email: &str,
        ip_address: Option<&str>,
        server_id: Uuid,
        server_name: &str,
        tool_name: &str,
        call_trace_id: &str,
        logged_arguments: Option<&serde_json::Value>,
        started: std::time::Instant,
        response: &JsonRpcResponse,
        stream_audit_body: Option<&str>,
    ) {
        let (status, error_message) = if let Some(ref err) = response.error {
            ("error".to_string(), Some(err.message.clone()))
        } else {
            ("ok".to_string(), None)
        };
        use think_watch_common::audit::{AuditActor, BodyCaptureStatus, McpActor};
        let actor = McpActor {
            user_id,
            user_email,
            ip: ip_address,
        };

        // Body capture for audit. arguments + upstream result land in
        // dedicated `mcp_logs.tool_arguments` / `mcp_logs.tool_result`
        // columns (separate from the metadata-only `detail` JSON) so
        // auditors can query them without parsing JSON per row.
        let dc = &self.dynamic_config;
        let capture_args = dc.audit_capture_tool_arguments().await;
        let capture_result = dc.audit_capture_tool_results().await;
        let body_max = dc.audit_body_max_bytes().await as usize;
        let redact_pii = dc.audit_body_redact_pii().await;
        // Snapshot the hot-swappable redactor ONCE per request so the
        // arguments + result halves see the same pattern set even if
        // the operator hot-swaps mid-call.
        let blob_redactor_snapshot = self.blob_redactor.load_full();
        let (arg_str, arg_bytes, result_str, result_bytes, capture_status) = if !capture_args
            && !capture_result
        {
            (
                None,
                None,
                None,
                None,
                Some(BodyCaptureStatus::Disabled.as_str().to_owned()),
            )
        } else {
            let mut truncated = false;
            let mut offloaded = false;
            let mut arg_bytes: Option<u32> = None;
            let mut result_bytes: Option<u32> = None;
            let arg_str = if capture_args {
                match logged_arguments {
                    Some(v) => {
                        let raw = serde_json::to_string(v)
                            .unwrap_or_else(|_| "[serialize_error]".to_owned());
                        arg_bytes = Some(raw.len() as u32);
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact_pii,
                                &blob_redactor_snapshot,
                                &self.blob_store,
                                call_trace_id,
                                "arguments",
                                &mut truncated,
                                &mut offloaded,
                            )
                            .await,
                        )
                    }
                    None => None,
                }
            } else {
                None
            };
            let result_str = if capture_result {
                // Prefer the FULL streaming-event sequence (when the
                // upstream used text/event-stream and emitted
                // progress notifications + a final response) over
                // just the final `result` field. Auditors replaying
                // a long-running tool execution need the whole
                // timeline, not just the punchline.
                let raw_opt: Option<String> = match (stream_audit_body, response.result.as_ref()) {
                    (Some(stream), _) => Some(stream.to_owned()),
                    (None, Some(v)) => Some(
                        serde_json::to_string(v).unwrap_or_else(|_| "[serialize_error]".to_owned()),
                    ),
                    (None, None) => None,
                };
                match raw_opt {
                    Some(raw) => {
                        result_bytes = Some(raw.len() as u32);
                        Some(
                            apply_mcp_body_capture(
                                raw,
                                body_max,
                                redact_pii,
                                &blob_redactor_snapshot,
                                &self.blob_store,
                                call_trace_id,
                                "result",
                                &mut truncated,
                                &mut offloaded,
                            )
                            .await,
                        )
                    }
                    None => None,
                }
            } else {
                None
            };
            let status = if arg_str.is_none() && result_str.is_none() {
                BodyCaptureStatus::Disabled
            } else if offloaded {
                // Same dominant-status rule as the AI gateway: a
                // single emit carrying one offloaded field reports
                // `offloaded` even if another field was small enough
                // to truncate.
                BodyCaptureStatus::Offloaded
            } else if truncated {
                BodyCaptureStatus::Truncated
            } else {
                BodyCaptureStatus::Captured
            };
            (
                arg_str,
                arg_bytes,
                result_str,
                result_bytes,
                Some(status.as_str().to_owned()),
            )
        };

        let mut entry = actor
            .audit("tools.call")
            .trace_id(call_trace_id.to_owned())
            .detail(serde_json::json!({
                "server_id": server_id.to_string(),
                "server_name": server_name,
                "tool_name": tool_name,
                "arguments": logged_arguments,
                "duration_ms": started.elapsed().as_millis() as i64,
                "status": status,
                "error_message": error_message,
            }));
        if let Some(a) = arg_str {
            entry = entry.request_body(a);
        }
        if let Some(r) = result_str {
            entry = entry.response_body(r);
        }
        // Stamp ORIGINAL byte counts (pre-offload) so the audit row's
        // `arguments_bytes` / `result_bytes` columns reflect the
        // user's actual payload size. Without these, an offloaded
        // tool result would report ~80 bytes (the s3:// URL length)
        // and break "average tool result size" analytics.
        if let Some(b) = arg_bytes {
            entry = entry.request_body_bytes(b);
        }
        if let Some(b) = result_bytes {
            entry = entry.response_body_bytes(b);
        }
        if let Some(s) = capture_status {
            entry = entry.body_capture_status(s);
        }
        self.audit.log(entry);
    }
}
