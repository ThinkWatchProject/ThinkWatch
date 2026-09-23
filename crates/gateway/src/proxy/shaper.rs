//! The last step before bytes reach the client: put the caller's model
//! name back and paint their PII back in.
//!
//! Both run on **client-format bytes**, after any dialect conversion, so
//! a passthrough response and a converted one go through the same code.
//!
//! **Model name.** A route can send `gpt-4` to `gpt-4o-2024-08-06`; the
//! caller asked for the alias and gets the alias back. Every format puts
//! the model in one of three places — top level (chat, every chunk),
//! `message.model` (Anthropic `message_start`), `response.model`
//! (Responses) — so rewriting those three covers all of them without
//! asking which format this is.
//!
//! **PII.** A whole response has its placeholders intact and is restored
//! in one pass. A stream does not: `{{EMA` can end one frame and `IL_1}}`
//! start the next, and between them sits `"}}]}\n\ndata: {"choices":…` —
//! the placeholder is not contiguous in the byte stream. So restoration
//! happens on the text field of each frame, with a restorer that holds
//! back an unclosed `{{` until the rest arrives.

use serde_json::Value;
use tw_dialect::frame::{self, Decoder, Frame};

use crate::pii_redactor::{PiiStreamRestorer, RedactionContext};

/// Rewrite the model name in a whole (non-streaming) response.
pub fn rewrite_model(body: &[u8], model: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    if set_model(&mut v, model) {
        serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
    } else {
        body.to_vec()
    }
}

/// Returns whether anything changed.
fn set_model(v: &mut Value, model: &str) -> bool {
    let mut changed = false;
    for path in ["/model", "/message/model", "/response/model"] {
        if let Some(slot) = v.pointer_mut(path)
            && slot.is_string()
            && slot.as_str() != Some(model)
        {
            *slot = Value::String(model.to_string());
            changed = true;
        }
    }
    changed
}

/// Reshapes a client-format SSE stream frame by frame.
pub struct StreamShaper {
    decoder: Decoder,
    model: String,
    restorer: Option<PiiStreamRestorer>,
}

impl StreamShaper {
    pub fn new(model: String, redaction: &RedactionContext) -> Self {
        let restorer = PiiStreamRestorer::new(redaction);
        Self {
            decoder: Decoder::default(),
            model,
            restorer: (!restorer.is_noop()).then_some(restorer),
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.decoder.feed(chunk);
        self.write(frames)
    }

    /// The stream ended. Emits whatever the decoder was still holding.
    pub fn finish(&mut self) -> Vec<u8> {
        let frames = self.decoder.flush();
        self.write(frames)
    }

    fn write(&mut self, frames: Vec<Frame>) -> Vec<u8> {
        let mut out = String::new();
        for f in frames {
            self.frame(f, &mut out);
        }
        out.into_bytes()
    }

    fn frame(&mut self, f: Frame, out: &mut String) {
        let Ok(mut v) = serde_json::from_str::<Value>(&f.data) else {
            // `[DONE]` and anything else that is not JSON. A held-back
            // tail has to go out before the stream's own terminator.
            if let Some(tail) = self.drain() {
                out.push_str(&frame::data(&chat_text_chunk(&self.model, &tail)));
            }
            out.push_str(&raw(&f));
            return;
        };

        set_model(&mut v, &self.model);

        if self.restorer.is_some() {
            if let Some(text) = text_delta_mut(&mut v) {
                if let Some(r) = self.restorer.as_mut() {
                    *text = r.process(text);
                }
            } else {
                // A frame that closes a text run: release anything held
                // back first, as a delta of its own, so it lands inside
                // the block it belongs to.
                if closes_text(&v)
                    && let Some(tail) = self.drain()
                {
                    out.push_str(&synthetic_delta(&v, &self.model, &tail));
                }
                // Frames that carry the whole text again (`output_text.done`,
                // `response.completed`) hold complete placeholders.
                if let Some(r) = self.restorer.as_ref() {
                    walk_strings(&mut v, &mut |s| *s = r.restore_oneshot(s));
                }
            }
        }

        out.push_str(&match &f.event {
            Some(e) => frame::named(e, &v),
            None => frame::data(&v),
        });
    }

    fn drain(&mut self) -> Option<String> {
        let tail = self.restorer.as_mut()?.flush();
        (!tail.is_empty()).then_some(tail)
    }
}

/// The streamed text in a frame, in whichever format it is.
fn text_delta_mut(v: &mut Value) -> Option<&mut String> {
    match v.get("type").and_then(Value::as_str) {
        // Anthropic
        Some("content_block_delta") => {
            let d = v.get_mut("delta")?;
            if d.get("type").and_then(Value::as_str) != Some("text_delta") {
                return None;
            }
            string_mut(d.get_mut("text")?)
        }
        // Responses
        Some("response.output_text.delta") => string_mut(v.get_mut("delta")?),
        Some(_) => None,
        // Chat has no `type`
        None => {
            let choice = v.get_mut("choices")?.get_mut(0)?;
            string_mut(choice.get_mut("delta")?.get_mut("content")?)
        }
    }
}

fn string_mut(v: &mut Value) -> Option<&mut String> {
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// Does this frame end a run of text?
fn closes_text(v: &Value) -> bool {
    match v.get("type").and_then(Value::as_str) {
        Some("content_block_stop") | Some("response.output_text.done") => true,
        Some(_) => false,
        None => v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .is_some_and(|f| !f.is_null()),
    }
}

/// A text delta carrying `tail`, shaped like the frame it precedes.
fn synthetic_delta(closing: &Value, model: &str, tail: &str) -> String {
    match closing.get("type").and_then(Value::as_str) {
        Some("content_block_stop") => frame::named(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": closing.get("index").cloned().unwrap_or(Value::from(0)),
                "delta": { "type": "text_delta", "text": tail },
            }),
        ),
        Some("response.output_text.done") => frame::named(
            "response.output_text.delta",
            &serde_json::json!({
                "type": "response.output_text.delta",
                "item_id": closing.get("item_id").cloned().unwrap_or(Value::Null),
                "output_index": closing.get("output_index").cloned().unwrap_or(Value::from(0)),
                "content_index": closing.get("content_index").cloned().unwrap_or(Value::from(0)),
                "delta": tail,
            }),
        ),
        _ => frame::data(&chat_text_chunk(model, tail)),
    }
}

fn chat_text_chunk(model: &str, text: &str) -> Value {
    serde_json::json!({
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }],
    })
}

fn raw(f: &Frame) -> String {
    match &f.event {
        Some(e) => format!("event: {e}\ndata: {}\n\n", f.data),
        None => format!("data: {}\n\n", f.data),
    }
}

fn walk_strings(v: &mut Value, f: &mut impl FnMut(&mut String)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(items) => items.iter_mut().for_each(|i| walk_strings(i, f)),
        Value::Object(map) => map.values_mut().for_each(|c| walk_strings(c, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ctx(pairs: &[(&str, &str)]) -> RedactionContext {
        RedactionContext {
            replacements: pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn frames(bytes: &[u8]) -> Vec<Value> {
        let mut d = Decoder::default();
        let mut fs = d.feed(bytes);
        fs.extend(d.flush());
        fs.into_iter()
            .filter_map(|f| serde_json::from_str(&f.data).ok())
            .collect()
    }

    fn chat_chunk(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"model":"gpt-4o-2024-08-06","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
        )
    }

    #[test]
    fn a_whole_response_gets_the_callers_model_back() {
        let body = br#"{"id":"x","model":"gpt-4o-2024-08-06","choices":[]}"#;
        let v: Value = serde_json::from_slice(&rewrite_model(body, "gpt-4")).unwrap();
        assert_eq!(v["model"], "gpt-4");
    }

    #[test]
    fn the_model_is_found_in_all_three_places_formats_put_it() {
        for (body, path) in [
            (r#"{"model":"up"}"#, "/model"),
            (r#"{"message":{"model":"up"}}"#, "/message/model"),
            (r#"{"response":{"model":"up"}}"#, "/response/model"),
        ] {
            let v: Value =
                serde_json::from_slice(&rewrite_model(body.as_bytes(), "alias")).unwrap();
            assert_eq!(v.pointer(path).unwrap(), "alias", "{body}");
        }
    }

    #[test]
    fn every_streamed_chunk_gets_the_callers_model_back() {
        let mut s = StreamShaper::new("gpt-4".into(), &ctx(&[]));
        let mut out = s.process(chat_chunk("hi").as_bytes());
        out.extend(s.process(chat_chunk(" there").as_bytes()));
        out.extend(s.finish());
        for f in frames(&out) {
            assert_eq!(f["model"], "gpt-4", "{f}");
        }
    }

    #[test]
    fn a_placeholder_split_across_two_frames_is_restored() {
        // Exactly why this cannot be done on bytes: frame structure sits
        // between the two halves.
        let mut s = StreamShaper::new("m".into(), &ctx(&[("{{EMAIL_1}}", "a@x.com")]));
        let mut out = s.process(chat_chunk("mail {{EMA").as_bytes());
        out.extend(s.process(chat_chunk("IL_1}} now").as_bytes()));
        out.extend(s.finish());
        let text: String = frames(&out)
            .iter()
            .filter_map(|f| {
                f["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "mail a@x.com now");
    }

    #[test]
    fn an_anthropic_text_delta_is_restored() {
        let mut s = StreamShaper::new("m".into(), &ctx(&[("{{EMAIL_1}}", "a@x.com")]));
        let ev = |d: Value| format!("event: content_block_delta\ndata: {d}\n\n");
        let mut out = s.process(
            ev(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"to {{EMAIL_"}})).as_bytes(),
        );
        out.extend(s.process(
            ev(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1}}"}})).as_bytes(),
        ));
        out.extend(s.finish());
        let text: String = frames(&out)
            .iter()
            .filter_map(|f| f["delta"]["text"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, "to a@x.com");
    }

    #[test]
    fn a_held_back_tail_is_released_before_the_block_closes() {
        // Text ending in an unclosed `{{` is not a placeholder: it goes out
        // verbatim, inside the block it belongs to, not after the block ends.
        let mut s = StreamShaper::new("m".into(), &ctx(&[("{{EMAIL_1}}", "a@x.com")]));
        let ev = |name: &str, d: Value| format!("event: {name}\ndata: {d}\n\n");
        let mut out = s.process(
            ev("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"literal {{"}})).as_bytes(),
        );
        out.extend(
            s.process(
                ev(
                    "content_block_stop",
                    serde_json::json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        out.extend(s.finish());
        let fs = frames(&out);
        let text: String = fs
            .iter()
            .filter_map(|f| f["delta"]["text"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, "literal {{");
        assert_eq!(fs.last().unwrap()["type"], "content_block_stop", "{fs:?}");
    }

    #[test]
    fn a_frame_carrying_the_whole_text_again_is_restored_too() {
        // Responses repeats the whole text in output_text.done and
        // response.completed.
        let mut s = StreamShaper::new("m".into(), &ctx(&[("{{EMAIL_1}}", "a@x.com")]));
        let out = s.process(
            format!(
                "event: response.output_text.done\ndata: {}\n\n",
                serde_json::json!({"type":"response.output_text.done","text":"mail {{EMAIL_1}}"})
            )
            .as_bytes(),
        );
        assert_eq!(frames(&out)[0]["text"], "mail a@x.com");
    }

    #[test]
    fn done_passes_through_untouched() {
        let mut s = StreamShaper::new("m".into(), &ctx(&[]));
        let out = s.process(b"data: [DONE]\n\n");
        assert_eq!(out, b"data: [DONE]\n\n");
    }
}
