use crate::pii_redactor::PiiStreamRestorer;
use crate::providers::traits::{ChatCompletionChunk, GatewayError, Usage};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Outcome classification for a finished stream. Re-exported from
/// the shared lifecycle module so the AI gateway and the MCP gateway
/// agree on a single audit-status / Prometheus-label set.
pub use think_watch_common::lifecycle::streaming::StreamOutcome;

/// Serialize a chunk for an SSE `data:` line. On serialization failure
/// — which should be impossible for a well-formed `ChatCompletionChunk`
/// but is theoretically reachable if a provider injects a non-finite
/// number into `usage` — emit a structured error event instead of an
/// empty `data:\n\n` frame. An empty event silently breaks audit
/// (zero-length chunks look successful) and confuses tolerant SSE
/// parsers; the explicit error frame is loud at every layer.
pub(crate) fn serialize_sse_chunk<T: serde::Serialize>(chunk: &T) -> String {
    match serde_json::to_string(chunk) {
        Ok(s) => s,
        Err(e) => {
            metrics::counter!("gateway_stream_chunk_serialize_failed_total").increment(1);
            tracing::error!("SSE chunk serialization failed: {e}");
            r#"{"error":{"message":"chunk serialization failed","type":"internal_error"}}"#
                .to_string()
        }
    }
}

/// Payload delivered to the `on_done` callback when a stream completes
/// (naturally or via client cancellation).
pub struct StreamResult {
    /// The most recent `Usage` value any chunk reported (`None` when the
    /// upstream never surfaced usage — common without
    /// `stream_options.include_usage`).
    pub usage: Option<Usage>,
    /// Every chunk observed before the stream ended.  For a natural
    /// completion this is the full sequence; for a cancellation it is a
    /// partial prefix.  Empty when the stream errored on the very first
    /// chunk.
    pub chunks: Vec<ChatCompletionChunk>,
    /// `true` when the upstream stream ran to its natural `[DONE]`
    /// sentinel.  `false` on client disconnect or mid-stream error.
    /// Kept for back-compat with existing `on_done` consumers; new
    /// code should consult `outcome` for the split.
    pub natural_completion: bool,
    /// Structured reason the stream ended. The natural completion bool
    /// is just a cached `outcome.is_natural()` for callers that don't
    /// need the split.
    pub outcome: StreamOutcome,
}

/// Converts a stream of `ChatCompletionChunk` results into an Axum
/// SSE response, returning the response alongside a oneshot
/// `Receiver<StreamResult>` that resolves **exactly once** when the
/// stream terminates — natural EOF, upstream error, or client drop.
///
/// Callers wrap the receiver into a lifecycle tail future:
///
/// ```ignore
/// let (sse, result_rx) = stream_to_sse_with_restorer(stream, restorer);
/// let response = sse.into_response();
/// let tail = Box::pin(async move {
///     let result = result_rx.await.expect("internal task always sends");
///     /* build Invoked<S> from result + ctx */
/// });
/// Invocation::Streaming { response, tail }
/// ```
///
/// Each chunk is serialized as `data: {json}\n\n`. When the source
/// stream ends, a final `data: [DONE]\n\n` event is emitted to signal
/// completion (matching the OpenAI streaming protocol).
///
/// **Why the channel dance:** the obvious implementation (await
/// `done_tx.send(...)` at the bottom of an `async_stream::stream!`
/// block) silently leaks accounting whenever the consumer (Sse)
/// drops the stream future before the loop exits — and the consumer
/// drops as soon as the client disconnects. A detached
/// `tokio::spawn` listens for either the stream's "I'm finished"
/// signal or the dropped sender that signals "I was cancelled" and
/// forwards the corresponding `StreamResult` to the returned
/// receiver. Either way, the receiver fires exactly once with
/// whatever state the stream had captured.
///
/// `restorer` runs each chunk's `delta.content` through a
/// `PiiStreamRestorer` (holds back any trailing content that might
/// still be growing into a placeholder; flushes the tail as a
/// synthetic chunk on completion). When `None` this is an exact
/// no-op — no extra allocations, no latency penalty for the
/// feature-off path.
pub fn stream_to_sse_with_restorer(
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    restorer: Option<PiiStreamRestorer>,
) -> (
    Sse<impl Stream<Item = Result<Event, Infallible>>>,
    tokio::sync::oneshot::Receiver<StreamResult>,
) {
    // Shared state — the stream loop writes into these; the post-flight
    // task reads them on completion or drop.
    let last_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let last_usage_for_done = last_usage.clone();
    let collected_chunks: Arc<Mutex<Vec<ChatCompletionChunk>>> =
        Arc::new(Mutex::new(Vec::with_capacity(64)));
    let chunks_for_done = collected_chunks.clone();

    // `done_tx.send(outcome)` runs from the stream loop on graceful
    // exit (Natural) or after observing a stream Err (UpstreamError).
    // If the loop is dropped before reaching either line, the sender
    // is dropped and the receiver yields `Err(RecvError)` — which we
    // map to ClientCancelled. Either way the spawned task assembles
    // a `StreamResult` and forwards it to the caller's receiver.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<StreamResult>();

    tokio::spawn(async move {
        let received = done_rx.await;
        let usage = last_usage_for_done.lock().ok().and_then(|mut g| g.take());
        let chunks = chunks_for_done
            .lock()
            .ok()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let outcome = received.unwrap_or(StreamOutcome::ClientCancelled);
        metrics::counter!(
            "gateway_stream_completion_total",
            "outcome" => outcome.metric_label()
        )
        .increment(1);
        let natural = outcome.is_natural();
        // `result_tx.send` drops the value silently when the receiver
        // is gone — that's the caller having dropped the tail future
        // (e.g. axum dropped the request). Nothing to clean up
        // ourselves; the StreamResult goes with it.
        let _ = result_tx.send(StreamResult {
            usage,
            chunks,
            natural_completion: natural,
            outcome,
        });
    });

    // Strip out the no-op case so the hot loop can skip the restorer
    // branch without re-checking every chunk.
    let mut restorer = restorer.filter(|r| !r.is_noop());
    // The very last chunk model+id+object we saw — needed if we have
    // to synthesise a final flush chunk for the restorer tail.
    let last_chunk_template: Arc<Mutex<Option<ChatCompletionChunk>>> = Arc::new(Mutex::new(None));

    let body = async_stream::stream! {
        let mut source = stream;
        let mut done_tx = Some(done_tx);

        // We need StreamExt::next() but importing it pollutes the
        // outer scope; pull it in lexically here.
        use futures::stream::StreamExt;
        while let Some(result) = source.next().await {
            match result {
                Ok(mut chunk) => {
                    // Capture usage off any chunk that carries it.
                    if chunk.usage.is_some()
                        && let Ok(mut g) = last_usage.lock()
                    {
                        *g = chunk.usage.clone();
                    }

                    // Collect a clone of each chunk for post-flight
                    // cache assembly — but cap retention so a 32k-token
                    // completion doesn't hold 32k cloned chunks in
                    // memory for the stream's lifetime. Beyond the cap
                    // we stop collecting; `assemble_response` (and
                    // the cache write that depends on it) becomes a
                    // no-op for over-long responses, which is the
                    // intended trade-off — the cache hit rate on
                    // truly large completions is low enough that the
                    // memory cliff isn't worth it.
                    const MAX_CACHED_CHUNKS: usize = 2048;
                    if let Ok(mut g) = collected_chunks.lock()
                        && g.len() < MAX_CACHED_CHUNKS
                    {
                        g.push(chunk.clone());
                    }

                    // PII restoration
                    if let Some(r) = restorer.as_mut() {
                        for choice in chunk.choices.iter_mut() {
                            if let Some(s) = choice.delta
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                            {
                                let restored = r.process(&s);
                                choice.delta["content"] =
                                    serde_json::Value::String(restored);
                            }
                        }
                        if let Ok(mut g) = last_chunk_template.lock() {
                            *g = Some(chunk.clone());
                        }
                    }

                    let json = serialize_sse_chunk(&chunk);
                    yield Ok::<Event, Infallible>(Event::default().data(json));
                }
                Err(e) => {
                    tracing::warn!("Stream error, forwarding as SSE error event: {e}");
                    // Pull the canonical status + label off the
                    // GatewayError so on_done logs the actual cause
                    // (429 stays a 429, 504 stays a 504) instead of
                    // the old blanket 502.
                    let error_type = e.error_tag().to_string();
                    let status_code = e.status_code();
                    let raw_message = e.to_string();
                    // Restore PII placeholders inside the error message
                    // before yielding. Upstream errors that include
                    // request fragments would otherwise leak `{{EMAIL_1}}`
                    // (or whatever the redactor uses) to the client
                    // instead of the original value the caller actually
                    // sent. `restore_oneshot` leaves the restorer's
                    // buffer alone so the subsequent tail flush below
                    // still behaves correctly.
                    let message = match restorer.as_ref() {
                        Some(r) if !r.is_noop() => r.restore_oneshot(&raw_message),
                        _ => raw_message.clone(),
                    };
                    let error_json = serde_json::json!({
                        "error": {
                            "message": message,
                            "type": "stream_error",
                            "error_type": error_type,
                        }
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default().data(error_json.to_string()),
                    );
                    // Bail the loop — once the upstream errors, the
                    // remaining chunks are usually a wash. We also
                    // need to send the outcome before the stream
                    // future is dropped, otherwise on_done would
                    // misclassify this as a client cancellation.
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(StreamOutcome::UpstreamError {
                            error_type,
                            // Audit/metrics keep the raw form — the
                            // restored copy is for the client only.
                            message: raw_message,
                            status_code,
                        });
                    }
                    break;
                }
            }
        }

        // Restorer flush — release any tail that got held back
        if let Some(r) = restorer.as_mut() {
            let tail = r.flush();
            if !tail.is_empty()
                && let Some(mut flush_chunk) = last_chunk_template
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
            {
                flush_chunk.usage = None;
                for choice in flush_chunk.choices.iter_mut() {
                    choice.delta = serde_json::json!({"content": tail});
                    choice.finish_reason = None;
                }
                let json = serialize_sse_chunk(&flush_chunk);
                yield Ok::<Event, Infallible>(Event::default().data(json));
            }
        }

        // Source stream is fully drained — natural completion.
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(StreamOutcome::Natural);
        }

        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };

    (Sse::new(body).keep_alive(KeepAlive::default()), result_rx)
}

/// Assemble a complete `ChatCompletionResponse` from a sequence of
/// streaming chunks.  Returns `None` if the chunks list is empty.
///
/// The assembled response concatenates all `delta.content` fields
/// into a single `message.content`, preserves `finish_reason` from
/// the last chunk that carries one, and attaches the provided `usage`.
pub fn assemble_response(
    chunks: &[ChatCompletionChunk],
    usage: Option<Usage>,
) -> Option<crate::providers::traits::ChatCompletionResponse> {
    let first = chunks.first()?;

    // Accumulate per-choice content and finish_reason.
    let mut choice_contents: std::collections::HashMap<u32, (String, Option<String>)> =
        std::collections::HashMap::new();

    for chunk in chunks {
        for cc in &chunk.choices {
            let entry = choice_contents
                .entry(cc.index)
                .or_insert_with(|| (String::new(), None));
            if let Some(content) = cc.delta.get("content").and_then(|v| v.as_str()) {
                entry.0.push_str(content);
            }
            if cc.finish_reason.is_some() {
                entry.1 = cc.finish_reason.clone();
            }
        }
    }

    let mut choices: Vec<crate::providers::traits::Choice> = choice_contents
        .into_iter()
        .map(
            |(idx, (content, finish_reason))| crate::providers::traits::Choice {
                index: idx,
                message: crate::providers::traits::ChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(content),
                    ..Default::default()
                },
                finish_reason,
            },
        )
        .collect();
    choices.sort_by_key(|c| c.index);

    Some(crate::providers::traits::ChatCompletionResponse {
        id: first.id.clone(),
        object: "chat.completion".to_string(),
        created: first.created,
        model: first.model.clone(),
        choices,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatCompletionChunk, Usage};
    use axum::body::Bytes;
    use axum::response::IntoResponse;
    use futures::StreamExt;

    fn chunk(usage: Option<Usage>) -> Result<ChatCompletionChunk, GatewayError> {
        Ok(ChatCompletionChunk {
            id: "test".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![],
            usage,
        })
    }

    /// Client drop mid-stream MUST still resolve the result receiver,
    /// carrying whatever usage / chunks the stream observed before the
    /// drop. The pump tail future relies on this — without it,
    /// post-call accounting would leak for any cancelled request.
    #[tokio::test]
    async fn result_receiver_resolves_when_client_drops_stream_early() {
        let producer = async_stream::stream! {
            yield chunk(Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            }));
            std::future::pending::<()>().await;
            #[allow(unreachable_code)]
            yield chunk(None);
        };

        let (sse, result_rx) = stream_to_sse_with_restorer(Box::pin(producer), None);

        let mut body_stream = sse.into_response().into_body().into_data_stream();
        let _first: Option<Result<Bytes, _>> = body_stream.next().await;
        drop(body_stream);

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), result_rx)
            .await
            .expect("receiver MUST resolve even when the client drops the stream early")
            .expect("internal task always sends");
        let usage = result.usage.expect("usage from the chunk we did observe");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert!(
            !result.natural_completion,
            "client cancel is not a natural completion"
        );
    }

    #[tokio::test]
    async fn result_receiver_resolves_on_natural_completion() {
        let producer = async_stream::stream! {
            yield chunk(Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            }));
        };

        let (sse, result_rx) = stream_to_sse_with_restorer(Box::pin(producer), None);
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        while let Some(item) = body_stream.next().await {
            let _: Result<Bytes, _> = item;
        }
        drop(body_stream);

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), result_rx)
            .await
            .expect("receiver MUST resolve after natural completion")
            .expect("internal task always sends");
        assert!(result.natural_completion);
        let usage = result.usage.expect("usage was reported");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[tokio::test]
    async fn result_receiver_yields_none_when_no_usage_was_seen() {
        let producer = async_stream::stream! {
            yield chunk(None);
        };

        let (sse, result_rx) = stream_to_sse_with_restorer(Box::pin(producer), None);
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        while let Some(item) = body_stream.next().await {
            let _: Result<Bytes, _> = item;
        }
        drop(body_stream);

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), result_rx)
            .await
            .expect("receiver MUST resolve after natural completion")
            .expect("internal task always sends");
        assert!(result.natural_completion);
        assert!(
            result.usage.is_none(),
            "no chunk reported usage ⇒ result carries None"
        );
    }
}
