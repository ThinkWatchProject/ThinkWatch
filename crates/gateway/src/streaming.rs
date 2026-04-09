use crate::providers::traits::{ChatCompletionChunk, GatewayError, Usage};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use std::convert::Infallible;
use std::pin::Pin;

/// Converts a stream of `ChatCompletionChunk` results into an Axum SSE response.
///
/// Each chunk is serialized as `data: {json}\n\n`. When the source stream ends,
/// a final `data: [DONE]\n\n` event is emitted to signal completion (matching
/// the OpenAI streaming protocol).
///
/// `on_done` runs **after** the source stream is fully drained but
/// **before** the SSE wrapper completes. It receives the most recent
/// `Usage` value the proxy saw on any chunk (None if no chunk
/// reported usage — common when the upstream doesn't include
/// `stream_options.include_usage`). The callback runs inside the
/// stream future so it stays in the request task; use it for
/// post-flight accounting (token-metric rate limits + budget caps).
pub fn stream_to_sse<F>(
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    on_done: F,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    F: FnOnce(Option<Usage>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + 'static,
{
    // Drive the source stream manually via async-stream so we can
    // tap each chunk for `usage` and run `on_done` AFTER the loop
    // exits — including the final `[DONE]` sentinel.
    let body = async_stream::stream! {
        let mut source = stream;
        let mut last_usage: Option<Usage> = None;

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
                    if chunk.usage.is_some() {
                        last_usage = chunk.usage.clone();
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

        // Source stream is fully drained. Run the post-flight
        // accounting callback before the [DONE] sentinel so the
        // counters are up to date by the time the client sees the
        // close. Errors here can't propagate to the client (the
        // request is already half-finished), so the callback is
        // expected to log internally.
        on_done(last_usage).await;

        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(body).keep_alive(KeepAlive::default())
}
