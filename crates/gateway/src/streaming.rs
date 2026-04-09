use crate::providers::traits::{ChatCompletionChunk, GatewayError, Usage};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Converts a stream of `ChatCompletionChunk` results into an Axum SSE response.
///
/// Each chunk is serialized as `data: {json}\n\n`. When the source stream ends,
/// a final `data: [DONE]\n\n` event is emitted to signal completion (matching
/// the OpenAI streaming protocol).
///
/// `on_done` is **guaranteed to run exactly once** for every stream
/// returned from this function — including the case where the client
/// drops the connection mid-stream. The callback receives the most
/// recent `Usage` value the proxy saw on any chunk (None if no chunk
/// reported usage — common when the upstream doesn't include
/// `stream_options.include_usage`).
///
/// **Why the channel dance:** the obvious implementation (call
/// `on_done().await` at the bottom of an `async_stream::stream!`
/// block) silently leaks accounting whenever the consumer (Sse)
/// drops the stream future before the loop exits — and the consumer
/// drops as soon as the client disconnects. We sidestep that by
/// running `on_done` in a detached `tokio::spawn` task that listens
/// for either the stream's "I'm finished" signal or the dropped
/// sender that signals "I was cancelled". Either way, the callback
/// fires exactly once with whatever usage the stream had captured.
pub fn stream_to_sse<F>(
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    on_done: F,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    F: FnOnce(Option<Usage>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + 'static,
{
    // Shared state — the stream loop writes the latest usage in,
    // the post-flight task reads it on completion or drop.
    let last_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let last_usage_for_done = last_usage.clone();

    // `done_tx.send(natural)` runs from the stream loop on graceful
    // exit. If the loop is dropped before reaching that line, the
    // sender is dropped and the receiver yields `Err(RecvError)` —
    // which we treat as "client cancelled mid-stream". Either way
    // the spawned task wakes up and runs `on_done`.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();

    tokio::spawn(async move {
        let outcome = done_rx.await;
        let usage = last_usage_for_done.lock().ok().and_then(|mut g| g.take());
        match outcome {
            Ok(true) => {
                // Natural completion — the stream ran to the [DONE]
                // sentinel. Counters track real provider usage.
                metrics::counter!("gateway_stream_completion_total", "outcome" => "natural")
                    .increment(1);
            }
            Ok(false) | Err(_) => {
                // Either the stream errored mid-flight or the client
                // disconnected before the [DONE] sentinel. We still
                // run accounting so the partial usage we captured
                // (if any) gets recorded — never let early disconnect
                // be a free-quota loophole.
                metrics::counter!("gateway_stream_completion_total", "outcome" => "cancelled")
                    .increment(1);
            }
        }
        on_done(usage).await;
    });

    let body = async_stream::stream! {
        let mut source = stream;
        let mut done_tx = Some(done_tx);

        // We need StreamExt::next() but importing it pollutes the
        // outer scope; pull it in lexically here.
        use futures::stream::StreamExt;
        while let Some(result) = source.next().await {
            match result {
                Ok(chunk) => {
                    // Capture usage off any chunk that carries it.
                    // OpenAI streaming with `stream_options.include_usage = true`
                    // emits one chunk near the end whose `usage` is set;
                    // Anthropic emits cumulative usage on the last
                    // `message_delta` event. Either way we just take
                    // the most recent non-None value.
                    if chunk.usage.is_some()
                        && let Ok(mut g) = last_usage.lock()
                    {
                        *g = chunk.usage.clone();
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

        // Source stream is fully drained. Tell the post-flight task
        // it was a natural completion; if this fails (the spawned
        // task already got cancelled or dropped) there's nothing to
        // do — accounting will run from the Drop path instead.
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(true);
        }

        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(body).keep_alive(KeepAlive::default())
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
        // Producer that yields ONE chunk with usage and then waits
        // forever — simulates an upstream that's mid-stream when
        // the client disconnects.
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
            move |usage: Option<Usage>| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if let Some(u) = usage {
                        cap_p.store(u.prompt_tokens, Ordering::SeqCst);
                        cap_c.store(u.completion_tokens, Ordering::SeqCst);
                    }
                })
            };

        let sse = stream_to_sse(Box::pin(producer), on_done);

        // Pull the SSE body once to make sure the producer's first
        // chunk gets observed (and last_usage gets populated). Then
        // drop the entire SSE wrapper to simulate the client going
        // away mid-stream.
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        let _first: Option<Result<Bytes, _>> = body_stream.next().await;
        drop(body_stream);

        // The post-flight task is detached. Give the runtime a few
        // ticks to drive it before asserting.
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
        // Producer yields one chunk with usage, then ends.
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
            move |usage: Option<Usage>| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if let Some(u) = usage {
                        cap_p.store(u.prompt_tokens, Ordering::SeqCst);
                        cap_c.store(u.completion_tokens, Ordering::SeqCst);
                    }
                })
            };

        let sse = stream_to_sse(Box::pin(producer), on_done);
        let mut body_stream = sse.into_response().into_body().into_data_stream();
        // Drain the entire body — natural completion.
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
            move |usage: Option<Usage>| -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    on_done_flag.store(true, Ordering::SeqCst);
                    if usage.is_some() {
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
