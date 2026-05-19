//! Real-time SSE pass-through for MCP `tools/call`. Owns the wire
//! parser (`find_sse_event_terminator`, `extract_sse_data_payload`),
//! the response-envelope picker, the body builder that forwards
//! chunks AS THEY ARRIVE, and the buffered-replay fallback that
//! `handle_tools_call` falls through to when streaming isn't
//! engaged.

use super::McpProxy;
use super::jsonrpc::{INTERNAL_ERROR, JsonRpcRequest, JsonRpcResponse, err_response};
use crate::cache::CallerScope;
use crate::registry::ServerCacheScope;
use uuid::Uuid;

/// Outcome of `McpProxy::handle_request`. The transport layer
/// renders these differently:
///
/// * `Buffered` → either `Json(response)` (client wants JSON) or a
///   single SSE event wrapping `response` (client wants SSE);
/// * `Streaming` → an `axum::response::sse::Sse` body that pumps
///   upstream chunks downstream as they arrive, with audit emission
///   running in a detached `on_done` task.
pub enum HandleOutcome {
    Buffered(JsonRpcResponse),
    Streaming(StreamingPayload),
}

/// Wraps an SSE-shaped body the transport layer will hand to
/// `axum::response::sse::Sse::new`.
pub struct StreamingPayload {
    pub body: std::pin::Pin<
        Box<
            dyn futures::stream::Stream<
                    Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                > + Send,
        >,
    >,
    pub new_session_id: Option<String>,
}

/// How an upstream stream terminated. Drives audit-time classification
/// and circuit breaker accounting in the detached on-done task.
pub(crate) enum StreamOutcome {
    /// Stream drained to end-of-body without error.
    Natural,
    /// The underlying transport (bytes_stream) returned an error
    /// mid-flight, OR the upstream replied with a non-2xx before
    /// any chunks could be forwarded.
    UpstreamError { message: String },
    /// `done_tx` was dropped without sending — the producing future
    /// terminated before reaching its sentinel send, which happens
    /// when the downstream client disconnects and axum drops the
    /// SSE body. Treated as success for the breaker (no upstream
    /// fault) but as not-cacheable (we never saw the full response).
    ClientCancelled,
}

/// Find the byte index in `s` immediately AFTER a complete SSE
/// event terminator (`\n\n` or `\r\n\r\n`). Returns `None` when no
/// terminator has arrived yet so the caller knows to keep buffering.
pub(crate) fn find_sse_event_terminator(s: &str) -> Option<usize> {
    let lf = s.find("\n\n").map(|i| i + 2);
    let crlf = s.find("\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Concatenate the `data:` payload(s) of one SSE event block. Per the
/// SSE spec, multiple `data:` lines in one event join with `\n`; non-
/// `data:` lines (`event:`, `id:`, `retry:`, comments) are ignored.
/// Returns `None` when the event carried no data payload — caller
/// skips it rather than yielding an empty downstream event.
pub(crate) fn extract_sse_data_payload(event_block: &str) -> Option<String> {
    let mut out = String::new();
    let mut had_data = false;
    for line in event_block.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            if had_data {
                out.push('\n');
            }
            out.push_str(payload.trim_start());
            had_data = true;
        }
    }
    had_data.then_some(out)
}

/// Pick the JSON-RPC response envelope from a sequence of upstream
/// events, mirroring `pool::parse_sse_json_rpc`'s id-matching rules so
/// the streaming and buffered paths agree on what counts as "the
/// response" (vs `notifications/progress` events that the upstream
/// emitted during tool execution).
pub(crate) fn pick_response_envelope(
    events: &[serde_json::Value],
    request_id: Option<&serde_json::Value>,
) -> Option<JsonRpcResponse> {
    let pick = |with_id: bool| -> Option<serde_json::Value> {
        events
            .iter()
            .rev()
            .find(|e| {
                let has_result_or_error = e.get("result").is_some() || e.get("error").is_some();
                if !has_result_or_error {
                    return false;
                }
                if with_id {
                    request_id
                        .map(|rid| e.get("id").map(|id| id == rid).unwrap_or(false))
                        .unwrap_or(false)
                } else {
                    true
                }
            })
            .cloned()
    };
    let matched = pick(true).or_else(|| pick(false))?;
    serde_json::from_value(matched).ok()
}

impl McpProxy {
    /// Build a `StreamingPayload` that pumps upstream SSE chunks
    /// downstream AS THEY ARRIVE, while accumulating every event
    /// envelope into a shared buffer that a detached on-done task
    /// drains to run circuit breaker accounting, cache write, and
    /// audit emission exactly once when the stream terminates (or
    /// the client disconnects).
    ///
    /// The on-done task runs on graceful end-of-body (Natural), on
    /// a bytes_stream error (UpstreamError), or on client drop
    /// (ClientCancelled — `done_tx` is dropped when the producing
    /// future is). It is the single audit-emit site for this code
    /// path; the synchronous tail in `handle_tools_call` is skipped
    /// when we return here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_chunk_passthrough(
        &self,
        upstream_resp: reqwest::Response,
        request_id: Option<serde_json::Value>,
        user_id: Uuid,
        user_email: String,
        ip_address: Option<String>,
        server_id: Uuid,
        server_name: String,
        tool_name: String,
        call_trace_id: String,
        upstream_request: JsonRpcRequest,
        logged_arguments: Option<serde_json::Value>,
        started: std::time::Instant,
        cache_scope_kind: ServerCacheScope,
        cache_account_label: Option<String>,
        effective_cache_ttl: u64,
    ) -> StreamingPayload {
        use futures::stream::StreamExt;
        use std::sync::{Arc, Mutex};

        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> =
            Arc::new(Mutex::new(Vec::with_capacity(8)));
        let events_for_done = events_buf.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();

        // The on-done task gets its own owned clone of every piece of
        // state it needs — McpProxy is `Clone` so the breaker / cache /
        // audit handles all come along for free.
        let proxy = self.clone();
        let server_name_done = server_name.clone();
        let request_id_done = request_id.clone();
        tokio::spawn(async move {
            let outcome = done_rx.await.unwrap_or(StreamOutcome::ClientCancelled);
            let events = events_for_done
                .lock()
                .ok()
                .map(|mut g| std::mem::take(&mut *g))
                .unwrap_or_default();
            let response = pick_response_envelope(&events, request_id_done.as_ref())
                .unwrap_or_else(|| {
                    let msg = match &outcome {
                        StreamOutcome::Natural => {
                            "Upstream stream ended without a response envelope".to_string()
                        }
                        StreamOutcome::UpstreamError { message } => {
                            format!("Upstream stream error: {message}")
                        }
                        StreamOutcome::ClientCancelled => {
                            "Client cancelled before upstream replied".to_string()
                        }
                    };
                    err_response(request_id_done.clone(), INTERNAL_ERROR, msg)
                });

            // Circuit breaker — transport error → failure; otherwise
            // follow the same server-side-vs-caller-side rule the
            // buffered path uses so the breaker doesn't open on a
            // single user's bad INVALID_PARAMS.
            match &outcome {
                StreamOutcome::UpstreamError { .. } => {
                    proxy
                        .circuit_breakers
                        .record_failure(&server_name_done)
                        .await;
                }
                StreamOutcome::Natural | StreamOutcome::ClientCancelled => {
                    proxy
                        .record_breaker_for_response(&server_name_done, &response)
                        .await;
                }
            }

            // Cache write only on a fully drained, successful stream.
            // Client cancellation means we may have a partial view of
            // the response so caching it would poison subsequent calls.
            if effective_cache_ttl > 0
                && response.error.is_none()
                && matches!(outcome, StreamOutcome::Natural)
            {
                let cache_scope = match cache_scope_kind {
                    ServerCacheScope::Global => None,
                    ServerCacheScope::PerCaller => Some(CallerScope {
                        user_id: &user_id,
                        account_label: cache_account_label.as_deref(),
                    }),
                };
                proxy
                    .cache
                    .set(
                        &server_id,
                        cache_scope,
                        &upstream_request,
                        &response,
                        effective_cache_ttl,
                    )
                    .await;
            }

            // Audit emit with the full upstream event timeline — same
            // shape the buffered path produces via parse_sse_json_rpc,
            // so trace replay UI gets identical data regardless of
            // which transport the call took.
            let stream_audit_body = serde_json::to_string(&events).ok();
            proxy
                .emit_tools_call_audit(
                    user_id,
                    &user_email,
                    ip_address.as_deref(),
                    server_id,
                    &server_name_done,
                    &tool_name,
                    &call_trace_id,
                    logged_arguments.as_ref(),
                    started,
                    &response,
                    stream_audit_body.as_deref(),
                )
                .await;
        });

        // The body itself: SSE chunk-by-chunk pass-through. Buffers
        // bytes only until the next `\n\n` boundary, then yields one
        // downstream event per upstream event. axum's `Sse` wrapper
        // re-frames each yielded `Event::default().data(payload)` as
        // `data: <payload>\n\n` on the wire.
        let bytes_source = upstream_resp
            .bytes_stream()
            .map(|r| r.map_err(|e| e.to_string()));
        let body = build_passthrough_body(bytes_source, events_buf, done_tx);

        StreamingPayload {
            body,
            // Proxy session-id is set by the transport layer from the
            // per-request session it owns; we don't override it here.
            new_session_id: None,
        }
    }
}

/// Pump bytes from `source` downstream as discrete SSE events,
/// accumulating each parsed envelope into `events_buf` for the
/// on-done audit pass. On natural end-of-stream sends `Natural` via
/// `done_tx`; on a source-error sends `UpstreamError`; if neither
/// path runs (e.g. the consumer drops the stream mid-flight) the
/// receiver sees `Err` and treats it as `ClientCancelled`.
///
/// Generic over the source so the production path (reqwest's
/// `bytes_stream` mapped to `String` errors) and tests (an
/// `iter`-backed Stream) share one implementation.
pub(crate) fn build_passthrough_body<S>(
    source: S,
    events_buf: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    done_tx: tokio::sync::oneshot::Sender<StreamOutcome>,
) -> std::pin::Pin<
    Box<
        dyn futures::stream::Stream<
                Item = Result<axum::response::sse::Event, std::convert::Infallible>,
            > + Send,
    >,
>
where
    S: futures::stream::Stream<Item = Result<bytes::Bytes, String>> + Send + 'static,
{
    use axum::response::sse::Event;
    use std::convert::Infallible;
    let body = async_stream::stream! {
        use futures::stream::StreamExt;
        let source = source;
        futures::pin_mut!(source);
        let mut text_buf = String::new();
        let mut done_tx = Some(done_tx);
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(bytes) => {
                    let s = String::from_utf8_lossy(&bytes);
                    text_buf.push_str(&s);
                    while let Some(end) = find_sse_event_terminator(&text_buf) {
                        let event_block: String = text_buf.drain(..end).collect();
                        if let Some(payload) = extract_sse_data_payload(&event_block)
                            && !payload.is_empty()
                        {
                            // Best-effort JSON parse so non-JSON
                            // `data:` payloads (rare) still land in
                            // the timeline verbatim.
                            let parsed = serde_json::from_str::<serde_json::Value>(&payload)
                                .unwrap_or_else(|_| {
                                    serde_json::Value::String(payload.clone())
                                });
                            if let Ok(mut g) = events_buf.lock() {
                                g.push(parsed);
                            }
                            yield Ok::<Event, Infallible>(Event::default().data(payload));
                        }
                    }
                }
                Err(message) => {
                    // Transport-level error mid-stream. Surface via
                    // the on-done task (no spec-defined error event
                    // shape to emit downstream).
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(StreamOutcome::UpstreamError { message });
                    }
                    break;
                }
            }
        }
        // Defensive flush: some upstreams omit the final `\n\n`.
        let trailing = text_buf.trim_end_matches(['\n', '\r']);
        if !trailing.is_empty()
            && let Some(payload) = extract_sse_data_payload(trailing)
            && !payload.is_empty()
        {
            let parsed = serde_json::from_str::<serde_json::Value>(&payload)
                .unwrap_or_else(|_| serde_json::Value::String(payload.clone()));
            if let Ok(mut g) = events_buf.lock() {
                g.push(parsed);
            }
            yield Ok::<Event, Infallible>(Event::default().data(payload));
        }
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(StreamOutcome::Natural);
        }
    };
    Box::pin(body)
}

/// Convert a (possibly already-streamed-by-upstream) JsonRpcResponse
/// into an SSE payload the transport layer can hand straight to
/// `axum::response::sse::Sse::new`.
///
/// `stream_audit_body` is the JSON array of upstream events the pool
/// captured in commit cb50ea3 — present when the upstream replied
/// with `text/event-stream`. We parse it back into discrete envelopes
/// and replay each as one SSE event. When absent, the upstream was
/// plain JSON and we emit one synthesized event with the response.
pub(crate) fn build_replay_payload(
    response: JsonRpcResponse,
    stream_audit_body: Option<&str>,
) -> StreamingPayload {
    use axum::response::sse::Event;
    use std::convert::Infallible;
    let mut events: Vec<String> = Vec::new();
    if let Some(audit_json) = stream_audit_body
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(audit_json)
    {
        for ev in arr {
            // Serialize each event back to a compact JSON line. The
            // SSE wire format wraps it in `data: ...\n\n` for us.
            if let Ok(s) = serde_json::to_string(&ev) {
                events.push(s);
            }
        }
    }
    // Fallback: no audit body OR parse failed — emit the final
    // response as a single event. Client still gets a spec-compliant
    // SSE shape with one envelope.
    if events.is_empty()
        && let Ok(s) = serde_json::to_string(&response)
    {
        events.push(s);
    }
    let stream = futures::stream::iter(
        events
            .into_iter()
            .map(|s| Ok::<_, Infallible>(Event::default().data(s))),
    );
    StreamingPayload {
        body: Box::pin(stream),
        new_session_id: None,
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use futures::StreamExt;

    fn make_response(id: u64, result: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(serde_json::json!(id)),
            result: Some(result),
            error: None,
        }
    }

    async fn drain(payload: StreamingPayload) -> Vec<String> {
        // SSE Event doesn't expose its data publicly; we serialize
        // via Debug to confirm content survived round-trip — tests
        // assert the COUNT.
        let mut stream = payload.body;
        let mut count = Vec::new();
        while let Some(ev) = stream.next().await {
            let serialized = format!("{:?}", ev.unwrap());
            count.push(serialized);
        }
        count
    }

    #[tokio::test]
    async fn plain_upstream_response_yields_single_event() {
        let resp = make_response(1, serde_json::json!({"content": "ok"}));
        let payload = build_replay_payload(resp, None);
        let events = drain(payload).await;
        assert_eq!(events.len(), 1, "single buffered response → one SSE event");
    }

    #[tokio::test]
    async fn streamed_upstream_replays_each_event() {
        let audit_json = serde_json::to_string(&serde_json::json!([
            {"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":33}},
            {"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":66}},
            {"jsonrpc":"2.0","id":1,"result":{"content":"done"}}
        ]))
        .unwrap();
        let resp = make_response(1, serde_json::json!({"content": "done"}));
        let payload = build_replay_payload(resp, Some(&audit_json));
        let events = drain(payload).await;
        assert_eq!(
            events.len(),
            3,
            "every upstream SSE event should replay downstream"
        );
    }

    #[tokio::test]
    async fn malformed_audit_body_falls_back_to_response() {
        let resp = make_response(1, serde_json::json!({"x": 1}));
        let payload = build_replay_payload(resp, Some("not json"));
        let events = drain(payload).await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn empty_audit_array_falls_back_to_response() {
        let resp = make_response(1, serde_json::json!({"y": 2}));
        let payload = build_replay_payload(resp, Some("[]"));
        let events = drain(payload).await;
        assert_eq!(events.len(), 1);
    }
}

#[cfg(test)]
mod sse_parser_tests {
    use super::*;

    #[test]
    fn terminator_lf_lf() {
        assert_eq!(find_sse_event_terminator("data: x\n\n"), Some(9));
        assert_eq!(find_sse_event_terminator("data: x\n\nmore"), Some(9));
    }

    #[test]
    fn terminator_crlf_crlf() {
        assert_eq!(find_sse_event_terminator("data: x\r\n\r\n"), Some(11));
    }

    #[test]
    fn terminator_picks_first_boundary() {
        let s = "a\nb\n\nc\r\n\r\n";
        assert_eq!(find_sse_event_terminator(s), Some(5));
    }

    #[test]
    fn terminator_none_when_incomplete() {
        assert_eq!(find_sse_event_terminator("data: still buffering"), None);
        assert_eq!(find_sse_event_terminator("data: x\n"), None);
    }

    #[test]
    fn extract_single_data_line() {
        assert_eq!(
            extract_sse_data_payload("data: hello"),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn extract_joins_multiple_data_lines() {
        let block = "data: first\ndata: second";
        assert_eq!(
            extract_sse_data_payload(block),
            Some("first\nsecond".to_owned())
        );
    }

    #[test]
    fn extract_strips_optional_space_after_colon() {
        assert_eq!(
            extract_sse_data_payload("data:no-space"),
            Some("no-space".to_owned())
        );
    }

    #[test]
    fn extract_ignores_other_sse_fields() {
        let block = "event: message\nid: 42\nretry: 1000\ndata: payload\n: comment";
        assert_eq!(extract_sse_data_payload(block), Some("payload".to_owned()));
    }

    #[test]
    fn extract_none_when_no_data_lines() {
        assert_eq!(extract_sse_data_payload("event: ping\nid: 1"), None);
    }

    #[test]
    fn pick_envelope_matches_request_id() {
        let req_id = serde_json::json!(7);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":50}}),
            serde_json::json!({"jsonrpc":"2.0","id":7,"result":{"content":"done"}}),
        ];
        let r = pick_response_envelope(&events, Some(&req_id)).expect("envelope");
        assert_eq!(r.id, Some(req_id));
        assert!(r.result.is_some());
    }

    #[test]
    fn pick_envelope_falls_back_to_last_response_shaped() {
        let req_id = serde_json::json!(7);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":50}}),
            serde_json::json!({"jsonrpc":"2.0","id":null,"result":{"content":"done"}}),
        ];
        let r = pick_response_envelope(&events, Some(&req_id)).expect("fallback envelope");
        assert!(r.result.is_some());
    }

    #[test]
    fn pick_envelope_none_when_only_notifications() {
        let req_id = serde_json::json!(1);
        let events = vec![
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":10}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":20}}),
        ];
        assert!(pick_response_envelope(&events, Some(&req_id)).is_none());
    }
}

#[cfg(test)]
mod passthrough_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream::{self, StreamExt};
    use std::sync::{Arc, Mutex};

    async fn run_body(
        chunks: Vec<Result<Bytes, String>>,
    ) -> (usize, Vec<serde_json::Value>, Option<&'static str>) {
        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
        let source = stream::iter(chunks);
        let body = build_passthrough_body(source, events_buf.clone(), done_tx);
        let mut count = 0usize;
        let mut body = body;
        while let Some(_ev) = body.next().await {
            count += 1;
        }
        let outcome = done_rx.await.ok().map(|o| match o {
            StreamOutcome::Natural => "natural",
            StreamOutcome::UpstreamError { .. } => "upstream_error",
            StreamOutcome::ClientCancelled => "cancelled",
        });
        let captured = events_buf.lock().unwrap().clone();
        (count, captured, outcome)
    }

    #[tokio::test]
    async fn one_event_per_chunk() {
        let chunks = vec![
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":33}}\n\n",
            )),
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":66}}\n\n",
            )),
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":\"done\"}}\n\n",
            )),
        ];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 3, "three upstream events → three downstream events");
        assert_eq!(events.len(), 3);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn event_split_across_two_chunks() {
        let chunks = vec![
            Ok(Bytes::from("data: {\"jsonrpc\":\"2.0\",\"id\":")),
            Ok(Bytes::from("1,\"result\":{\"x\":1}}\n\n")),
        ];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1, "fragmented event must coalesce into one yield");
        assert_eq!(events.len(), 1);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        let chunks = vec![Ok(Bytes::from("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"))];
        let (count, events, _outcome) = run_body(chunks).await;
        assert_eq!(count, 2, "back-to-back events in one chunk → two yields");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn trailing_event_without_final_terminator() {
        let chunks = vec![Ok(Bytes::from(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":1}}",
        ))];
        let (count, events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1, "trailing event flushed at EOF");
        assert_eq!(events.len(), 1);
        assert_eq!(outcome, Some("natural"));
    }

    #[tokio::test]
    async fn upstream_error_surfaces_via_oneshot() {
        let chunks = vec![
            Ok(Bytes::from(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\n",
            )),
            Err("network reset".to_owned()),
        ];
        let (count, _events, outcome) = run_body(chunks).await;
        assert_eq!(count, 1);
        assert_eq!(outcome, Some("upstream_error"));
    }

    #[tokio::test]
    async fn client_drop_yields_cancelled() {
        let events_buf: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
        let source = stream::iter(vec![Ok::<Bytes, String>(Bytes::from(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\n",
        ))])
        .chain(stream::pending());
        let body = build_passthrough_body(source, events_buf.clone(), done_tx);
        let mut body = body;
        let _ = body.next().await;
        drop(body);
        let outcome = done_rx.await.ok();
        assert!(outcome.is_none(), "dropped sender → recv error");
    }

    #[tokio::test]
    async fn non_json_payload_recorded_verbatim() {
        let chunks = vec![Ok(Bytes::from("data: not-json-text\n\n"))];
        let (count, events, _) = run_body(chunks).await;
        assert_eq!(count, 1);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], serde_json::Value::String(_)));
    }
}
