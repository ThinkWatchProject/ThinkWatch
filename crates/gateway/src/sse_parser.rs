//! Minimal SSE (Server-Sent Events) parser.
//!
//! Replaces `eventsource-stream` with ~60 lines of code to eliminate the
//! only HIGH-risk supply-chain dependency.
//!
//! Spec: https://html.spec.whatwg.org/multipage/server-sent-events.html

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

/// A parsed SSE event.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// The `event:` field (empty string if not set).
    pub event: String,
    /// The `data:` field (concatenated if multiple `data:` lines).
    pub data: String,
}

/// Stream adapter that parses raw bytes into `SseEvent`s.
pub struct SseStream<S> {
    inner: S,
    buffer: String,
}

impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
        }
    }
}

/// Extension trait to convert a bytes stream into an SSE event stream.
pub trait SseStreamExt: Stream<Item = Result<Bytes, reqwest::Error>> + Sized {
    fn sse_events(self) -> SseStream<Self> {
        SseStream::new(self)
    }
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>>> SseStreamExt for S {}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<SseEvent, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Try to extract a complete event from the buffer.
            // Events are delimited by a blank line (\n\n).
            if let Some(event) = try_parse_event(&mut this.buffer) {
                return Poll::Ready(Some(Ok(event)));
            }

            // Need more data from the underlying stream.
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    // Append raw bytes to buffer.
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        this.buffer.push_str(text);
                    }
                    // Loop back to try parsing again.
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    // Stream ended — try to flush any remaining data as a final event.
                    if !this.buffer.trim().is_empty() {
                        if let Some(event) = try_parse_event_force(&mut this.buffer) {
                            return Poll::Ready(Some(Ok(event)));
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Try to parse one SSE event from the front of the buffer.
/// Returns `None` if no complete event (terminated by `\n\n`) is available yet.
fn try_parse_event(buffer: &mut String) -> Option<SseEvent> {
    // Look for double newline (event boundary)
    let boundary = buffer.find("\n\n")?;
    let raw = buffer[..boundary].to_string();
    // Remove the consumed bytes + the two newlines
    buffer.drain(..boundary + 2);
    Some(parse_fields(&raw))
}

/// Force-parse whatever remains in the buffer as a final event.
fn try_parse_event_force(buffer: &mut String) -> Option<SseEvent> {
    let raw = std::mem::take(buffer);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(parse_fields(trimmed))
}

/// Parse SSE field lines into an `SseEvent`.
fn parse_fields(raw: &str) -> SseEvent {
    let mut event_type = String::new();
    let mut data_parts: Vec<&str> = Vec::new();

    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data_parts.push(value.strip_prefix(' ').unwrap_or(value));
        } else if let Some(value) = line.strip_prefix("event:") {
            event_type = value.strip_prefix(' ').unwrap_or(value).to_string();
        }
        // Ignore `id:`, `retry:`, and comments (lines starting with `:`)
    }

    SseEvent {
        event: event_type,
        data: data_parts.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_data() {
        let mut buf = "data: hello world\n\n".to_string();
        let event = try_parse_event(&mut buf).unwrap();
        assert_eq!(event.data, "hello world");
        assert_eq!(event.event, "");
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_event_with_type() {
        let mut buf = "event: message_start\ndata: {\"type\":\"start\"}\n\n".to_string();
        let event = try_parse_event(&mut buf).unwrap();
        assert_eq!(event.event, "message_start");
        assert_eq!(event.data, "{\"type\":\"start\"}");
    }

    #[test]
    fn parse_multi_line_data() {
        let mut buf = "data: line1\ndata: line2\n\n".to_string();
        let event = try_parse_event(&mut buf).unwrap();
        assert_eq!(event.data, "line1\nline2");
    }

    #[test]
    fn parse_multiple_events() {
        let mut buf = "data: first\n\ndata: second\n\n".to_string();
        let e1 = try_parse_event(&mut buf).unwrap();
        assert_eq!(e1.data, "first");
        let e2 = try_parse_event(&mut buf).unwrap();
        assert_eq!(e2.data, "second");
    }

    #[test]
    fn incomplete_event_returns_none() {
        let mut buf = "data: partial".to_string();
        assert!(try_parse_event(&mut buf).is_none());
    }

    #[test]
    fn done_marker() {
        let mut buf = "data: [DONE]\n\n".to_string();
        let event = try_parse_event(&mut buf).unwrap();
        assert_eq!(event.data, "[DONE]");
    }
}
