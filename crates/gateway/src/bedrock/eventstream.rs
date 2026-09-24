//! AWS eventstream → SSE.
//!
//! Bedrock's ConverseStream is not SSE but AWS eventstream binary frames:
//! each frame has a prelude (total length, header length, prelude CRC), typed
//! headers, a payload and a whole-frame CRC. Every other format streams SSE,
//! and everything downstream — format conversion, usage sniffing, stream
//! assembly — reads SSE. So **the frames become SSE where the bytes come in**,
//! and nothing after that needs to know Bedrock is different.
//!
//! The shape follows what `tw_dialect::bedrock::stream` reads: the
//! `:event-type` header goes into `event:`, the JSON payload into `data:`.
//!
//! **A frame can be cut on any byte**; the incomplete tail waits for the next
//! chunk. `aws-smithy-eventstream` checks the CRCs — a frame that fails is
//! broken, and nothing is guessed past it.

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use bytes::{Buf, BytesMut};

/// What went wrong in the stream.
#[derive(Debug)]
pub enum StreamError {
    /// A broken frame: wrong length or CRC
    Malformed(String),
    /// The upstream reported an error inside the stream (`:message-type` is
    /// `exception` or `error`), e.g. throttling halfway. `kind` is the AWS
    /// exception name
    Upstream { kind: String, message: String },
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Malformed(m) => write!(f, "malformed AWS eventstream frame: {m}"),
            StreamError::Upstream { kind, message } => write!(f, "{kind}: {message}"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Converts as the bytes arrive.
#[derive(Debug, Default)]
pub struct Transcoder {
    buf: BytesMut,
    decoder: MessageFrameDecoder,
}

impl Transcoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk; returns every frame it completed, written as SSE.
    ///
    /// After an error the transcoder must not be used again: the frame
    /// boundaries are lost.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, StreamError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let before = self.buf.remaining();
            let frame = self
                .decoder
                .decode_frame(&mut self.buf)
                .map_err(|e| StreamError::Malformed(e.to_string()))?;
            match frame {
                DecodedFrame::Complete(message) => {
                    let header = |name: &str| {
                        message
                            .headers()
                            .iter()
                            .find(|h| h.name().as_str() == name)
                            .and_then(|h| h.value().as_string().ok())
                            .map(|s| s.as_str().to_string())
                    };
                    let payload = String::from_utf8_lossy(message.payload());
                    match header(":message-type").as_deref() {
                        Some("event") | None => {
                            let event = header(":event-type").unwrap_or_default();
                            write_frame(&mut out, &event, &payload);
                        }
                        Some("exception") => {
                            return Err(StreamError::Upstream {
                                kind: header(":exception-type").unwrap_or_default(),
                                message: message_of(&payload),
                            });
                        }
                        Some(other) => {
                            return Err(StreamError::Upstream {
                                kind: header(":error-code").unwrap_or_else(|| other.to_string()),
                                message: header(":error-message").unwrap_or_default(),
                            });
                        }
                    }
                }
                // The buffer also shrinks when the prelude was read but the
                // frame is incomplete — only when nothing was consumed is it
                // really time to wait for the next chunk
                DecodedFrame::Incomplete if self.buf.remaining() == before => return Ok(out),
                DecodedFrame::Incomplete => {}
            }
        }
    }
}

/// Write one SSE event. The payload is split into one `data:` per line —
/// an SSE reader joins them with newlines, so multi-line JSON comes back intact.
fn write_frame(out: &mut Vec<u8>, event: &str, payload: &str) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.push(b'\n');
    for line in payload.split('\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// An exception payload is `{"message": "..."}`; anything else is returned as is.
fn message_of(payload: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("Message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};

    fn event(kind: &str, payload: &str) -> Vec<u8> {
        let m = Message::new(payload.as_bytes().to_vec())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String(kind.to_string().into()),
            ))
            .add_header(Header::new(
                ":content-type",
                HeaderValue::String("application/json".into()),
            ));
        let mut out = Vec::new();
        write_message_to(&m, &mut out).unwrap();
        out
    }

    fn exception(kind: &str, message: &str) -> Vec<u8> {
        let m = Message::new(format!(r#"{{"message":"{message}"}}"#).into_bytes())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("exception".into()),
            ))
            .add_header(Header::new(
                ":exception-type",
                HeaderValue::String(kind.to_string().into()),
            ));
        let mut out = Vec::new();
        write_message_to(&m, &mut out).unwrap();
        out
    }

    #[test]
    fn each_frame_becomes_one_sse_event() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"晴"}}"#,
        ));
        let sse = Transcoder::new().feed(&wire).unwrap();
        assert_eq!(
            String::from_utf8(sse).unwrap(),
            "event: messageStart\ndata: {\"role\":\"assistant\"}\n\n\
             event: contentBlockDelta\ndata: {\"contentBlockIndex\":0,\"delta\":{\"text\":\"晴\"}}\n\n"
        );
    }

    #[test]
    fn a_frame_cut_on_any_byte_comes_out_whole() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(event(
            "metadata",
            r#"{"usage":{"inputTokens":3,"outputTokens":5}}"#,
        ));
        let whole = Transcoder::new().feed(&wire).unwrap();
        // Byte by byte: the prelude, headers, payload and CRC all get cut
        let mut t = Transcoder::new();
        let mut pieced = Vec::new();
        for b in &wire {
            pieced.extend(t.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(pieced, whole);
    }

    #[test]
    fn an_exception_in_the_stream_is_an_error_not_an_event() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(exception("throttlingException", "Too many requests"));
        match Transcoder::new().feed(&wire) {
            Err(StreamError::Upstream { kind, message }) => {
                assert_eq!(kind, "throttlingException");
                assert_eq!(message, "Too many requests");
            }
            other => panic!("expected an upstream error, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupted_frame_is_refused() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        let n = wire.len();
        wire[n - 6] ^= 0xff; // one changed payload byte breaks the frame CRC
        assert!(matches!(
            Transcoder::new().feed(&wire),
            Err(StreamError::Malformed(_))
        ));
    }

    /// Unframing (here) and reading the frames (`tw_dialect`) are written
    /// apart and meet at one convention: `:event-type` into `event:`, the
    /// payload into `data:`. This walks the wire bytes all the way to a
    /// client's format — if either side changes the shape, it breaks.
    #[test]
    fn a_converse_stream_reaches_a_chat_client_with_its_text_and_usage() {
        use tw_dialect::convert::decode;
        use tw_dialect::ir::{Dialect, Target};

        let wire = [
            event("messageStart", r#"{"role":"assistant"}"#),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"sun"}}"#,
            ),
            event(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"ny"}}"#,
            ),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
            event(
                "metadata",
                r#"{"usage":{"inputTokens":60,"cacheReadInputTokens":40,"outputTokens":20,"totalTokens":120}}"#,
            ),
        ]
        .concat();

        // The client speaks Chat and is routed to Bedrock
        let body = serde_json::json!({
            "model": "anthropic.claude",
            "stream": true,
            "messages": [{"role": "user", "content": "weather?"}],
        });
        let converted = decode(Dialect::Chat, &body, "/v1/chat/completions", None)
            .unwrap()
            .encode(&Target {
                dialect: Dialect::Bedrock,
                official: true,
                default_max_tokens: 4096,
            });
        let mut to_client = converted.session.stream();
        let mut sniffer = tw_dialect::usage::Sniffer::new();

        // Seven bytes at a time, so frame boundaries land anywhere
        let mut transcoder = Transcoder::new();
        let mut chat = Vec::new();
        for chunk in wire.chunks(7) {
            let sse = transcoder.feed(chunk).unwrap();
            sniffer.feed(&sse);
            chat.extend(to_client.process(&sse));
        }
        chat.extend(to_client.finish());
        let chat = String::from_utf8(chat).unwrap();

        let text: String = chat
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
            .filter_map(|v| {
                v["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "sunny");
        assert!(chat.contains(r#""finish_reason":"stop""#), "{chat}");

        // Usage is already readable from the unframed SSE, before it is
        // converted to the client's format
        let usage = sniffer.finish().expect("usage");
        assert_eq!((usage.input, usage.cache_read, usage.output), (60, 40, 20));
    }
}
