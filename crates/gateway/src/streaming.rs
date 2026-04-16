use crate::pii_redactor::PiiStreamRestorer;
use crate::providers::traits::{ChatCompletionChunk, GatewayError, Usage};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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
    pub natural_completion: bool,
}

/// Converts a stream of `ChatCompletionChunk` results into an Axum SSE response.
///
/// Each chunk is serialized as `data: {json}\n\n`. When the source stream ends,
/// a final `data: [DONE]\n\n` event is emitted to signal completion (matching
/// the OpenAI streaming protocol).
///
/// `on_done` is **guaranteed to run exactly once** for every stream
/// returned from this function — including the case where the client
/// drops the connection mid-stream.
///
/// **Why the channel dance:** the obvious implementation (call
/// `on_done().await` at the bottom of an `async_stream::stream!`
/// block) silently leaks accounting whenever the consumer (Sse)
/// drops the stream future before the loop exits — and the consumer
/// drops as soon as the client disconnects. We sidestep that by
/// running `on_done` in a detached `tokio::spawn` task that listens
/// for either the stream's "I'm finished" signal or the dropped
/// sender that signals "I was cancelled". Either way, the callback
/// fires exactly once with whatever state the stream had captured.
pub fn stream_to_sse<F>(
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    on_done: F,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    F: FnOnce(StreamResult) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + 'static,
{
    stream_to_sse_with_restorer(stream, on_done, None)
}

/// Same as `stream_to_sse`, but runs each chunk's `delta.content` through
/// a `PiiStreamRestorer` first. The restorer holds back any trailing
/// content that might still be growing into a placeholder; on completion,
/// a final synthetic chunk flushes whatever is left in the buffer.
///
/// When `restorer` is `None` this is an exact no-op delegation — no
/// extra allocations, no latency penalty for the feature-off path.
pub fn stream_to_sse_with_restorer<F>(
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    on_done: F,
    restorer: Option<PiiStreamRestorer>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    F: FnOnce(StreamResult) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + 'static,
{
    // Shared state — the stream loop writes into these; the post-flight
    // task reads them on completion or drop.
    let last_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let last_usage_for_done = last_usage.clone();
    let collected_chunks: Arc<Mutex<Vec<ChatCompletionChunk>>> =
        Arc::new(Mutex::new(Vec::with_capacity(64)));
    let chunks_for_done = collected_chunks.clone();

    // `done_tx.send(natural)` runs from the stream loop on graceful
    // exit. If the loop is dropped before reaching that line, the
    // sender is dropped and the receiver yields `Err(RecvError)` —
    // which we treat as "client cancelled mid-stream". Either way
    // the spawned task wakes up and runs `on_done`.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();

    tokio::spawn(async move {
        let outcome = done_rx.await;
        let usage = last_usage_for_done.lock().ok().and_then(|mut g| g.take());
        let chunks = chunks_for_done
            .lock()
            .ok()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let natural = matches!(outcome, Ok(true));
        match outcome {
            Ok(true) => {
                metrics::counter!("gateway_stream_completion_total", "outcome" => "natural")
                    .increment(1);
            }
            Ok(false) | Err(_) => {
                metrics::counter!("gateway_stream_completion_total", "outcome" => "cancelled")
                    .increment(1);
            }
        }
        on_done(StreamResult {
            usage,
            chunks,
            natural_completion: natural,
        })
        .await;
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

                    // Collect a clone of each chunk for post-flight cache assembly.
                    if let Ok(mut g) = collected_chunks.lock() {
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

                    let json = serde_json::to_string(&chunk).unwrap_or_default();
                    yield Ok::<Event, Infallible>(Event::default().data(json));
                }
                Err(e) => {
                    tracing::warn!("Stream error, forwarding as SSE error event: {e}");
                    let error_json = serde_json::json!({
                        "error": {
                            "message": e.to_string(),
                            "type": "stream_error"
                        }
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default().data(error_json.to_string()),
                    );
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
                let json = serde_json::to_string(&flush_chunk).unwrap_or_default();
                yield Ok::<Event, Infallible>(Event::default().data(json));
            }
        }

        // Source stream is fully drained — natural completion.
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(true);
        }

        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(body).keep_alive(KeepAlive::default())
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

    #[tokio::test]
    async fn on_done_runs_when_client_drops_stream_early() {
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

        let on_done_called = Arc::new(AtomicBool::new(false));
        let captured_prompt = Arc::new(AtomicU32::new(0));
        let captured_completion = Arc::new(AtomicU32::new(0));
        let on_done_flag = on_done_called.clone();
        let cap_p = captured_prompt.clone();
        let cap_c = captured_completion.clone();

        let on_done =
            move |result: StreamResult| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if let Some(u) = result.usage {
                        cap_p.store(u.prompt_tokens, Ordering::SeqCst);
                        cap_c.store(u.completion_tokens, Ordering::SeqCst);
                    }
                })
            };

        let sse = stream_to_sse(Box::pin(producer), on_done);

        let mut body_stream = sse.into_response().into_body().into_data_stream();
        let _first: Option<Result<Bytes, _>> = body_stream.next().await;
        drop(body_stream);

        for _ in 0..20 {
            if on_done_called.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert!(
            on_done_called.load(Ordering::SeqCst),
            "on_done MUST run even when the client drops the stream early"
        );
        assert_eq!(captured_prompt.load(Ordering::SeqCst), 10);
        assert_eq!(captured_completion.load(Ordering::SeqCst), 20);
    }

    #[tokio::test]
    async fn on_done_runs_on_natural_completion() {
        let producer = async_stream::stream! {
            yield chunk(Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            }));
        };

        let on_done_called = Arc::new(AtomicBool::new(false));
        let captured_prompt = Arc::new(AtomicU32::new(0));
        let captured_completion = Arc::new(AtomicU32::new(0));
        let on_done_flag = on_done_called.clone();
        let cap_p = captured_prompt.clone();
        let cap_c = captured_completion.clone();

        let on_done =
            move |result: StreamResult| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if let Some(u) = result.usage {
                        cap_p.store(u.prompt_tokens, Ordering::SeqCst);
                        cap_c.store(u.completion_tokens, Ordering::SeqCst);
                    }
                })
            };

        let sse = stream_to_sse(Box::pin(producer), on_done);
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        while let Some(item) = body_stream.next().await {
            let _: Result<Bytes, _> = item;
        }
        drop(body_stream);

        for _ in 0..20 {
            if on_done_called.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert!(on_done_called.load(Ordering::SeqCst));
        assert_eq!(captured_prompt.load(Ordering::SeqCst), 5);
        assert_eq!(captured_completion.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn on_done_runs_with_none_when_no_usage_was_seen() {
        let producer = async_stream::stream! {
            yield chunk(None);
        };

        let on_done_called = Arc::new(AtomicBool::new(false));
        let received_some = Arc::new(AtomicBool::new(false));
        let on_done_flag = on_done_called.clone();
        let received = received_some.clone();

        let on_done =
            move |result: StreamResult| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if result.usage.is_some() {
                        received.store(true, Ordering::SeqCst);
                    }
                })
            };

        let sse = stream_to_sse(Box::pin(producer), on_done);
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        while let Some(item) = body_stream.next().await {
            let _: Result<Bytes, _> = item;
        }
        drop(body_stream);

        for _ in 0..20 {
            if on_done_called.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert!(on_done_called.load(Ordering::SeqCst));
        assert!(
            !received_some.load(Ordering::SeqCst),
            "on_done should receive None when no chunk reported usage"
        );
    }
}
