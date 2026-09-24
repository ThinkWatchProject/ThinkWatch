//! `GET /v1/responses` upgraded to a WebSocket: the Responses API over one
//! long-lived connection, the way Codex talks to OpenAI.
//!
//! # The protocol
//!
//! The client sends one text frame per turn, `{"type": "response.create",
//! …}`, carrying what the HTTP request body would (a nested `response`
//! object is read too). The server answers with the events the SSE stream
//! would carry — `response.created`, the deltas, `response.completed` or
//! `response.failed` — one JSON object per text frame. Turns on one
//! connection run one after another.
//!
//! # Each turn is a request, through the whole pipeline
//!
//! A WebSocket proxied as a pipe — frames copied between the client and an
//! upstream socket — skips everything the HTTP path does to a request:
//! limits and budgets, model access, content filter, PII redaction, tool-call
//! inspection, billing, the audit row. Here each `response.create` is handed
//! to the same `generate` an HTTP `POST /v1/responses` with `stream: true`
//! goes through, and its SSE is unwrapped into frames. So a turn is limited,
//! routed (to any upstream format, with conversion), inspected, billed and
//! logged exactly like the HTTP request it stands for, and the API key was
//! checked on the upgrade by the same middleware.
//!
//! What this does not carry over: a connection-local store of the previous
//! response. Each turn goes upstream as its own request, so a
//! `previous_response_id` works only against an upstream that stored that
//! response.
//!
//! A refusal — before the stream opens or during it — is a
//! `response.failed` frame, the event Responses clients dispatch on; the
//! connection stays open for the next turn. A client that goes away
//! mid-turn cancels that turn, recorded like a dropped HTTP stream.

use std::collections::VecDeque;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tw_dialect::frame::Decoder;
use tw_dialect::ir::Dialect;

use super::generate::{RESPONSES, generate};
use super::{GatewayRequestIdentity, GatewayState};

/// GET /v1/responses with `Upgrade: websocket`.
pub async fn proxy_responses_ws(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    metrics::counter!("gateway_responses_ws_connections_total").increment(1);
    ws.on_upgrade(move |socket| serve(socket, state, headers, identity))
}

/// How a turn ended, for the connection.
enum Turn {
    /// Answered, or refused with a frame: ready for the next.
    Done,
    /// The client is gone.
    Gone,
}

async fn serve(
    socket: WebSocket,
    state: GatewayState,
    headers: HeaderMap,
    identity: GatewayRequestIdentity,
) {
    let (mut tx, mut rx) = socket.split();
    // Turns the client sent while one was still running.
    let mut queued: VecDeque<Message> = VecDeque::new();
    loop {
        let message = match queued.pop_front() {
            Some(m) => m,
            None => match rx.next().await {
                Some(Ok(m)) => m,
                _ => break,
            },
        };
        let body = match message {
            Message::Text(t) => request_of(t.as_str()),
            Message::Binary(_) => Err("Send response.create as a text frame.".to_string()),
            Message::Close(_) => break,
            // axum answers pings itself.
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let turn = match body {
            Ok(body) => {
                let answer = generate(
                    state.clone(),
                    headers.clone(),
                    identity.clone(),
                    None,
                    Bytes::from(body),
                    RESPONSES,
                    "/v1/responses",
                    None,
                )
                .await;
                match answer {
                    Ok(resp) => relay(resp, &mut tx, &mut rx, &mut queued).await,
                    Err(e) => {
                        let e = e.error();
                        refuse(&mut tx, e.status_code(), &e.to_string()).await
                    }
                }
            }
            Err(why) => refuse(&mut tx, 400, &why).await,
        };
        if matches!(turn, Turn::Gone) {
            break;
        }
    }
    let _ = tx.close().await;
}

/// The request body a `response.create` frame stands for, as a stream.
fn request_of(text: &str) -> Result<Vec<u8>, String> {
    let not_create =
        || "Only response.create messages are accepted on this connection.".to_string();
    let Ok(Value::Object(mut frame)) = serde_json::from_str::<Value>(text) else {
        return Err(not_create());
    };
    if frame.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(not_create());
    }
    frame.remove("type");
    let mut body = match frame.remove("response") {
        Some(Value::Object(nested)) => nested,
        _ => frame,
    };
    body.insert("stream".into(), Value::Bool(true));
    Ok(Value::Object(body).to_string().into_bytes())
}

/// Send the turn's SSE to the client, a frame per event.
///
/// Dropping the response body cancels the turn: the pipeline's tail then
/// records it as cancelled by the client, as for an HTTP stream.
async fn relay(
    resp: axum::response::Response,
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    rx: &mut futures::stream::SplitStream<WebSocket>,
    queued: &mut VecDeque<Message>,
) -> Turn {
    let mut body = resp.into_body().into_data_stream();
    let mut decoder = Decoder::default();
    loop {
        tokio::select! {
            chunk = body.next() => {
                let (frames, end) = match chunk {
                    Some(Ok(bytes)) => (decoder.feed(&bytes), false),
                    _ => (decoder.flush(), true),
                };
                for f in frames {
                    // Every Responses event is a JSON object; nothing else
                    // is a frame.
                    if serde_json::from_str::<Value>(&f.data).is_err() {
                        continue;
                    }
                    if tx.send(Message::Text(f.data.into())).await.is_err() {
                        return Turn::Gone;
                    }
                }
                if end {
                    return Turn::Done;
                }
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Turn::Gone,
                Some(Ok(m @ (Message::Text(_) | Message::Binary(_)))) => queued.push_back(m),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A refused turn: `response.failed`, the connection stays open.
async fn refuse(
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    status: i64,
    message: &str,
) -> Turn {
    let status = u16::try_from(status).unwrap_or(502);
    let sse = tw_dialect::convert::error_frame(Dialect::Responses, status, message);
    let mut decoder = Decoder::default();
    let mut frames = decoder.feed(sse.as_bytes());
    frames.extend(decoder.flush());
    for f in frames {
        if tx.send(Message::Text(f.data.into())).await.is_err() {
            return Turn::Gone;
        }
    }
    Turn::Done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Value {
        serde_json::from_slice(&request_of(text).unwrap()).unwrap()
    }

    #[test]
    fn a_create_frame_is_the_request_body_as_a_stream() {
        let v = body(r#"{"type":"response.create","model":"m","input":"hi","stream":false}"#);
        assert_eq!(
            v,
            serde_json::json!({"model": "m", "input": "hi", "stream": true})
        );
    }

    #[test]
    fn a_nested_response_object_is_read_too() {
        let v = body(r#"{"type":"response.create","response":{"model":"m","input":"hi"}}"#);
        assert_eq!(v["model"], "m");
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn anything_but_a_create_frame_is_refused() {
        assert!(request_of(r#"{"type":"response.cancel"}"#).is_err());
        assert!(request_of("not json").is_err());
        assert!(request_of("[1]").is_err());
    }
}
