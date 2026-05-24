//! Minimal SSE (Server-Sent Events) parser.
//!
//! Replaces `eventsource-stream` with ~60 lines of code to eliminate the
//! only HIGH-risk supply-chain dependency.
//!
//! Spec: https://html.spec.whatwg.org/multipage/server-sent-events.html

use bytes::Bytes;
use futures::stream::Stream;
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

/// Cap on the in-memory buffer between SSE event boundaries (`\n\n`).
/// An upstream that streams without ever emitting the boundary would
/// otherwise grow `buffer` unbounded → OOM. 8 MiB is well above any
/// real LLM chunk (full responses are typically tens of KiB) but small
/// enough that an attacker can't pin a process. When exceeded the
/// stream surfaces an error and is dropped by the parser.
const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Stream adapter that parses raw bytes into `SseEvent`s.
pub struct SseStream<S> {
    inner: S,
    /// Decoded text accumulated between event boundaries (`\n\n`).
    buffer: String,
    /// Trailing bytes from the last upstream chunk that didn't form a
    /// complete UTF-8 sequence on their own. A multi-byte codepoint
    /// can land split across two TCP reads; without this, the prior
    /// `from_utf8` check silently dropped the entire chunk.
    pending_bytes: Vec<u8>,
    /// Sticky terminal state — once we surface an error we stop polling
    /// the inner stream. Avoids the consumer accidentally re-driving
    /// us back into the same error.
    errored: bool,
}

impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            pending_bytes: Vec::new(),
            errored: false,
        }
    }
}

/// Append a freshly-received byte chunk to `buffer`, carrying any
/// partial-UTF-8 tail through `pending_bytes`. Invalid sequences
/// (not merely incomplete) are replaced with U+FFFD so the stream
/// remains parseable rather than silently truncating.
///
/// Returns `Err` when appending the chunk would push the buffer over
/// [`MAX_SSE_EVENT_BYTES`]. The check happens BEFORE the allocation
/// so a single oversized chunk (e.g. a 50 MiB gzip-decoded SSE
/// payload) can't momentarily blow past the cap.
fn extend_buffer_with_chunk(
    buffer: &mut String,
    pending: &mut Vec<u8>,
    fresh: &[u8],
) -> Result<(), BufferOverflow> {
    if buffer.len() + pending.len() + fresh.len() > MAX_SSE_EVENT_BYTES {
        return Err(BufferOverflow);
    }
    // Merge any prior partial bytes with the fresh ones — placement
    // matters because the previous tail is the *start* of the new
    // codepoint.
    let mut staging: Vec<u8> = Vec::with_capacity(pending.len() + fresh.len());
    staging.extend_from_slice(pending);
    staging.extend_from_slice(fresh);
    pending.clear();

    let mut cursor = 0usize;
    while cursor < staging.len() {
        match std::str::from_utf8(&staging[cursor..]) {
            Ok(s) => {
                buffer.push_str(s);
                cursor = staging.len();
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // SAFETY: `valid_up_to` is by definition the length of
                // the longest valid UTF-8 prefix.
                let valid = unsafe {
                    std::str::from_utf8_unchecked(&staging[cursor..cursor + valid_up_to])
                };
                buffer.push_str(valid);
                cursor += valid_up_to;
                match e.error_len() {
                    None => {
                        // Trailing bytes form an *incomplete* codepoint —
                        // park them for the next chunk.
                        pending.extend_from_slice(&staging[cursor..]);
                        cursor = staging.len();
                    }
                    Some(invalid_len) => {
                        // Genuinely invalid bytes — emit the replacement
                        // codepoint and skip past them.
                        buffer.push('\u{FFFD}');
                        cursor += invalid_len;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Sentinel returned by [`extend_buffer_with_chunk`] when the
/// post-append size would exceed [`MAX_SSE_EVENT_BYTES`]. The caller
/// is expected to tear the stream down — there is no recovery once
/// an SSE event payload outgrows the cap.
#[derive(Debug)]
struct BufferOverflow;

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
        if this.errored {
            return Poll::Ready(None);
        }

        loop {
            // Try to extract a complete event from the buffer.
            // Events are delimited by a blank line (\n\n).
            if let Some(event) = try_parse_event(&mut this.buffer) {
                return Poll::Ready(Some(Ok(event)));
            }

            // Need more data from the underlying stream.
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    // Cap check happens INSIDE `extend_buffer_with_chunk`
                    // and runs BEFORE the allocation, so even a single
                    // chunk that would push us over `MAX_SSE_EVENT_BYTES`
                    // is rejected without ever being copied into the
                    // buffer. Tear the stream down on overflow.
                    if extend_buffer_with_chunk(&mut this.buffer, &mut this.pending_bytes, &bytes)
                        .is_err()
                    {
                        this.errored = true;
                        tracing::error!(
                            buffered_bytes = this.buffer.len() + this.pending_bytes.len(),
                            incoming_chunk_bytes = bytes.len(),
                            "SSE event would exceed {MAX_SSE_EVENT_BYTES} bytes — terminating stream"
                        );
                        metrics::counter!("gateway_sse_buffer_overflow_total").increment(1);
                        this.buffer.clear();
                        this.pending_bytes.clear();
                        return Poll::Ready(None);
                    }
                    // Loop back to try parsing again.
                }
                Poll::Ready(Some(Err(e))) => {
                    this.errored = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    // Stream ended — flush any leftover partial UTF-8
                    // as the replacement codepoint, then try a final
                    // force-parse so a graceful upstream that omits
                    // the trailing `\n\n` still yields its last event.
                    if !this.pending_bytes.is_empty() {
                        this.buffer.push('\u{FFFD}');
                        this.pending_bytes.clear();
                    }
                    if !this.buffer.trim().is_empty()
                        && let Some(event) = try_parse_event_force(&mut this.buffer)
                    {
                        return Poll::Ready(Some(Ok(event)));
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

    #[test]
    fn extend_buffer_handles_split_utf8_codepoint() {
        // U+4E2D (中) encodes as E4 B8 AD. Split it across two chunks
        // — the old `from_utf8` check silently dropped any chunk that
        // failed to decode, losing the entire fragment.
        let mut buf = String::new();
        let mut pending = Vec::new();
        extend_buffer_with_chunk(&mut buf, &mut pending, &[0xE4, 0xB8]).unwrap();
        // First chunk is incomplete — nothing emitted yet, bytes held.
        assert_eq!(buf, "");
        assert_eq!(pending, vec![0xE4, 0xB8]);
        extend_buffer_with_chunk(&mut buf, &mut pending, &[0xAD]).unwrap();
        assert_eq!(buf, "中");
        assert!(pending.is_empty());
    }

    #[test]
    fn extend_buffer_emits_replacement_for_invalid_utf8() {
        // 0xFF on its own is never valid UTF-8 — produce U+FFFD rather
        // than dropping it (or worse, dropping the whole containing
        // chunk like the previous implementation).
        let mut buf = String::new();
        let mut pending = Vec::new();
        extend_buffer_with_chunk(&mut buf, &mut pending, b"ok-").unwrap();
        extend_buffer_with_chunk(&mut buf, &mut pending, &[0xFF]).unwrap();
        extend_buffer_with_chunk(&mut buf, &mut pending, b"-done").unwrap();
        assert_eq!(buf, "ok-\u{FFFD}-done");
        assert!(pending.is_empty());
    }

    #[test]
    fn extend_buffer_rejects_oversized_chunk_before_allocating() {
        // The cap defends against a single oversized chunk (e.g. a
        // multi-MB gzip-decoded SSE payload) blowing past MAX_SSE_EVENT_BYTES
        // momentarily. Pin: when the post-append size would cross
        // the cap, the function returns Err WITHOUT mutating buffer
        // or pending.
        let mut buf = String::new();
        let mut pending = Vec::new();
        let oversized = vec![b'a'; MAX_SSE_EVENT_BYTES + 1];
        assert!(extend_buffer_with_chunk(&mut buf, &mut pending, &oversized).is_err());
        assert!(buf.is_empty());
        assert!(pending.is_empty());
    }
}
