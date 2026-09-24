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
//! # The previous response is kept on the connection
//!
//! OpenAI's socket mode keeps the connection's most recent response in
//! memory, so a turn can name it in `previous_response_id` even with
//! `store: false` — which is how Codex runs. The desktop gateway gets that
//! for free by piping the whole socket to one upstream socket. Here each
//! turn is a separate upstream request, possibly to an upstream in another
//! format with no such store, so the connection keeps it instead: the
//! conversation so far (the turn's full input and the output items of its
//! `response.completed`). A turn whose `previous_response_id` names it goes
//! upstream with that history written into `input` and no
//! `previous_response_id` — valid against any upstream, and billed as
//! what it is. Like OpenAI, only the most recent response is kept; a
//! `previous_response_id` naming anything else goes upstream as sent, for
//! an upstream that stored it.
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
    // The connection's most recent response.
    let mut last: Option<Last> = None;
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
            Ok(mut body) => {
                let history = continue_from(&mut body, last.as_ref());
                let answer = generate(
                    state.clone(),
                    headers.clone(),
                    identity.clone(),
                    None,
                    Bytes::from(Value::Object(body).to_string()),
                    RESPONSES,
                    "/v1/responses",
                    None,
                )
                .await;
                match answer {
                    Ok(resp) => {
                        let (turn, completed) = relay(resp, &mut tx, &mut rx, &mut queued).await;
                        // A failed turn leaves the chain where it was.
                        if let (Some(history), Some(done)) = (history, completed) {
                            last = Last::of(history, &done).or(last);
                        }
                        turn
                    }
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
fn request_of(text: &str) -> Result<serde_json::Map<String, Value>, String> {
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
    Ok(body)
}

/// A response this connection produced, as the conversation up to and
/// including it: every input item the turn went upstream with, then the
/// response's output items.
struct Last {
    id: String,
    items: Vec<Value>,
}

impl Last {
    /// From a turn's full input and its `response.completed` response.
    fn of(mut items: Vec<Value>, response: &Value) -> Option<Last> {
        let id = response.get("id")?.as_str()?.to_string();
        for item in response.get("output")?.as_array()? {
            let mut item = item.clone();
            // Output item ids refer to the upstream's store, which a
            // `store: false` turn never wrote to; sent back as input they
            // would be looked up and not found. `call_id` is what ties a
            // tool result to its call, and stays.
            if let Some(o) = item.as_object_mut() {
                o.remove("id");
                o.remove("status");
            }
            items.push(item);
        }
        Some(Last { id, items })
    }
}

/// The turn's `input` as a list of items: a bare string is one user
/// message.
fn input_items(body: &serde_json::Map<String, Value>) -> Vec<Value> {
    match body.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => {
            vec![serde_json::json!({"type": "message", "role": "user", "content": text})]
        }
        _ => Vec::new(),
    }
}

/// Continue from the connection's last response if the turn names it:
/// its history goes into `input` and `previous_response_id` goes away.
///
/// Returns the turn's whole conversation, to keep once it completes —
/// `None` when the turn still points at an earlier response this
/// connection does not have, so its history is not all here.
fn continue_from(
    body: &mut serde_json::Map<String, Value>,
    last: Option<&Last>,
) -> Option<Vec<Value>> {
    let previous = body.get("previous_response_id").and_then(Value::as_str);
    let mut items = match (previous, last) {
        (None, _) => Vec::new(),
        (Some(p), Some(l)) if p == l.id => l.items.clone(),
        (Some(_), _) => return None,
    };
    items.extend(input_items(body));
    if previous.is_some() {
        body.remove("previous_response_id");
        body.insert("input".into(), Value::Array(items.clone()));
    }
    Some(items)
}

/// Send the turn's SSE to the client, a frame per event, and hand back the
/// `response` of its `response.completed`, if it got that far.
///
/// Dropping the response body cancels the turn: the pipeline's tail then
/// records it as cancelled by the client, as for an HTTP stream.
async fn relay(
    resp: axum::response::Response,
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    rx: &mut futures::stream::SplitStream<WebSocket>,
    queued: &mut VecDeque<Message>,
) -> (Turn, Option<Value>) {
    let mut body = resp.into_body().into_data_stream();
    let mut decoder = Decoder::default();
    let mut completed = None;
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
                    let Ok(mut event) = serde_json::from_str::<Value>(&f.data) else {
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                        completed = event.get_mut("response").map(Value::take);
                    }
                    if tx.send(Message::Text(f.data.into())).await.is_err() {
                        return (Turn::Gone, None);
                    }
                }
                if end {
                    return (Turn::Done, completed);
                }
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return (Turn::Gone, None),
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
        Value::Object(request_of(text).unwrap())
    }

    fn create(v: Value) -> serde_json::Map<String, Value> {
        request_of(&v.to_string()).unwrap()
    }

    #[test]
    fn a_turn_naming_the_last_response_carries_its_history() {
        let mut first =
            create(serde_json::json!({"type": "response.create", "model": "m", "input": "one"}));
        let history = continue_from(&mut first, None).unwrap();
        let done = serde_json::json!({"id": "resp_1", "output": [
            {"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "hi"}]},
            {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
        ]});
        let last = Last::of(history, &done).unwrap();

        let mut second = create(serde_json::json!({"type": "response.create", "model": "m",
            "previous_response_id": "resp_1",
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "ok"}]}));
        let kept = continue_from(&mut second, Some(&last)).unwrap();
        assert!(second.get("previous_response_id").is_none());
        let input = second["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"], "one");
        assert_eq!(input[1]["role"], "assistant");
        assert!(input[1].get("id").is_none() && input[1].get("status").is_none());
        assert_eq!(input[2]["call_id"], "call_1");
        assert!(input[2].get("id").is_none());
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn a_response_this_connection_does_not_have_goes_upstream_as_sent() {
        let last = Last {
            id: "resp_1".into(),
            items: vec![serde_json::json!({"x": 1})],
        };
        let mut turn = create(serde_json::json!({"type": "response.create", "model": "m",
            "previous_response_id": "resp_0", "input": "two"}));
        assert!(continue_from(&mut turn, Some(&last)).is_none());
        assert_eq!(turn["previous_response_id"], "resp_0");
        assert_eq!(turn["input"], "two");
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
