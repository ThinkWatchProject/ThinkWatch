//! The last step before bytes reach the client: put the caller's model
//! name back and paint their PII back in.
//!
//! Both run on **client-format bytes**, after any dialect conversion, so
//! a passthrough response and a converted one go through the same code.
//!
//! **Model name.** A route can send `gpt-4` to `gpt-4o-2024-08-06`; the
//! caller asked for the alias and gets the alias back. Every format puts
//! the model in one of four places — top level (chat, every chunk),
//! `message.model` (Anthropic `message_start`), `response.model`
//! (Responses), `modelVersion` (Gemini) — so rewriting those covers all
//! of them without asking which format this is.
//!
//! **PII.** A whole response has its placeholders intact and is restored
//! in one pass (`pii_redactor::restore_body`). A stream does not: `{{EMA`
//! can end one frame and `IL_1}}` start the next, with frame structure in
//! between. Restoration happens per frame, on the text and the tool
//! arguments of whichever format this is, with one lane per content block
//! or tool call — thinkwatch-core's `FrameRestorer`, the same one the
//! desktop gateway uses.
//!
//! **Usage.** A Chat stream is always sent upstream asking for its usage
//! chunk, or there would be nothing to bill. When the caller did not ask
//! for it, the shaper takes it back out: the trailing chunk that carries
//! only `usage`, and the `"usage": null` the upstream adds to every other
//! chunk once asked.

use serde_json::Value;
use tw_dialect::frame::{self, Decoder, Frame};
use tw_dialect::ir::Dialect;
use tw_guard::redact::replace::Ledger;
use tw_guard::redact::sse::{FrameRestorer, Synth};

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
    for path in [
        "/model",
        "/message/model",
        "/response/model",
        "/modelVersion",
    ] {
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
    restorer: Option<FrameRestorer>,
    hide_usage: bool,
}

impl StreamShaper {
    pub fn new(model: String, redaction: &Ledger, client: Dialect) -> Self {
        let restorer = FrameRestorer::new(redaction, client);
        Self {
            decoder: Decoder::default(),
            model,
            restorer: (!restorer.is_noop()).then_some(restorer),
            hide_usage: false,
        }
    }

    /// Take the usage the caller did not ask for out of a Chat stream.
    pub fn hiding_usage(mut self, hide: bool) -> Self {
        self.hide_usage = hide;
        self
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.decoder.feed(chunk);
        self.write(frames).into_bytes()
    }

    /// The stream ended. Emits whatever the decoder was still holding, then
    /// any text held back waiting to be a placeholder.
    pub fn finish(&mut self) -> Vec<u8> {
        let frames = self.decoder.flush();
        let mut out = self.write(frames);
        self.drain(&mut out);
        out.into_bytes()
    }

    fn write(&mut self, frames: Vec<Frame>) -> String {
        let mut out = String::new();
        for f in frames {
            self.frame(f, &mut out);
        }
        out
    }

    fn frame(&mut self, f: Frame, out: &mut String) {
        let Ok(mut v) = serde_json::from_str::<Value>(&f.data) else {
            // `[DONE]` and anything else that is not JSON. A held-back
            // tail has to go out before the stream's own terminator.
            self.drain(out);
            out.push_str(&raw(&f));
            return;
        };
        if self.hide_usage
            && let Some(obj) = v.as_object_mut()
            && obj.remove("usage").is_some()
            && obj
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|c| c.is_empty())
        {
            // The usage chunk itself: nothing else in it.
            return;
        }
        if let Some(r) = self.restorer.as_mut() {
            for s in r.frame(&mut v).before {
                self.synth(s, out);
            }
        }
        set_model(&mut v, &self.model);
        out.push_str(&match &f.event {
            Some(e) => frame::named(e, &v),
            None => frame::data(&v),
        });
    }

    fn drain(&mut self, out: &mut String) {
        let Some(r) = self.restorer.as_mut() else {
            return;
        };
        for s in r.drain() {
            self.synth(s, out);
        }
    }

    fn synth(&self, mut s: Synth, out: &mut String) {
        set_model(&mut s.data, &self.model);
        out.push_str(&match &s.event {
            Some(e) => frame::named(e, &s.data),
            None => frame::data(&s.data),
        });
    }
}

/// Gemini's stream without `alt=sse`: one JSON array, an element per
/// chunk, sent as the chunks arrive.
///
/// The pipeline works on SSE throughout (see `generate::GEMINI_SSE`);
/// this is the last step, after everything else has read the frames. A
/// Gemini SSE frame and an array element carry the same object, so each
/// `data:` payload becomes one element. An error frame becomes an element
/// too, which is where Gemini itself puts a mid-stream error.
#[derive(Default)]
pub struct JsonArrayFramer {
    decoder: Decoder,
    opened: bool,
}

impl JsonArrayFramer {
    pub fn process(&mut self, sse: &[u8]) -> Vec<u8> {
        let frames = self.decoder.feed(sse);
        self.write(frames)
    }

    /// The stream ended: whatever the decoder held, then the closing
    /// bracket. An empty stream is still an array.
    pub fn finish(&mut self) -> Vec<u8> {
        let frames = self.decoder.flush();
        let mut out = self.write(frames);
        if !self.opened {
            out.push(b'[');
        }
        out.extend_from_slice(b"]");
        out
    }

    fn write(&mut self, frames: Vec<Frame>) -> Vec<u8> {
        let mut out = String::new();
        for f in frames {
            // `[DONE]` and anything else that is not an object has no
            // place in the array.
            if serde_json::from_str::<Value>(&f.data).is_err() {
                continue;
            }
            out.push_str(if self.opened { ",\r\n" } else { "[" });
            self.opened = true;
            out.push_str(&f.data);
        }
        out.into_bytes()
    }
}

fn raw(f: &Frame) -> String {
    match &f.event {
        Some(e) => format!("event: {e}\ndata: {}\n\n", f.data),
        None => format!("data: {}\n\n", f.data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger that issued `{{EMAIL_1}}` for `a@x.com`, or nothing.
    fn ctx(email: Option<&str>) -> Ledger {
        let r = crate::pii_redactor::PiiRedactor::from_config(&[
            think_watch_common::pii::PiiPatternConfig {
                name: "email".into(),
                regex: r"[a-z]+@x\.com".into(),
                placeholder_prefix: "EMAIL".into(),
            },
        ]);
        r.redact_str(email.unwrap_or("")).1
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
    fn a_gemini_sse_stream_becomes_one_json_array() {
        let mut f = JsonArrayFramer::default();
        let mut out = f.process(b"data: {\"a\":1}\n\ndata: {\"b\"");
        out.extend(f.process(b":2}\n\n"));
        out.extend(f.finish());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v, serde_json::json!([{"a": 1}, {"b": 2}]));

        let mut empty = JsonArrayFramer::default();
        assert_eq!(empty.finish(), b"[]");
    }

    #[test]
    fn a_gemini_answer_carries_the_callers_model() {
        let body = serde_json::json!({"candidates": [], "modelVersion": "gemini-2.5-pro-002"});
        let out = rewrite_model(body.to_string().as_bytes(), "my-alias");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["modelVersion"], "my-alias");
    }

    #[test]
    fn usage_the_caller_did_not_ask_for_is_taken_back_out() {
        let chunk = serde_json::json!({"model":"up","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null});
        let usage = serde_json::json!({"model":"up","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}});
        let stream = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");

        let mut s = StreamShaper::new("m".into(), &ctx(None), Dialect::Chat).hiding_usage(true);
        let mut out = s.process(stream.as_bytes());
        out.extend(s.finish());
        let fs = frames(&out);
        assert_eq!(fs.len(), 1, "{fs:?}");
        assert_eq!(fs[0]["choices"][0]["delta"]["content"], "hi");
        assert!(fs[0].get("usage").is_none(), "{}", fs[0]);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("data: [DONE]\n\n")
        );

        // Asked for: left alone.
        let mut s = StreamShaper::new("m".into(), &ctx(None), Dialect::Chat);
        let mut out = s.process(stream.as_bytes());
        out.extend(s.finish());
        let fs = frames(&out);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[1]["usage"]["total_tokens"], 4);
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
        let mut s = StreamShaper::new("gpt-4".into(), &ctx(None), Dialect::Chat);
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
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Chat);
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
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Anthropic);
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
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Anthropic);
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
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Responses);
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
    fn a_tool_calls_arguments_get_the_callers_pii_back() {
        // The old shaper restored text only: a model asked to "email
        // a@x.com" called the tool with `{{EMAIL_1}}` as the address.
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Chat);
        let call = |args: &str| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"model":"up","choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":args}}
                ]},"finish_reason":null}]})
            )
        };
        let mut out = s.process(call(r#"{"to":"{{EMA"#).as_bytes());
        out.extend(s.process(call(r#"IL_1}}"}"#).as_bytes()));
        out.extend(s.finish());
        let args: String = frames(&out)
            .iter()
            .filter_map(|f| {
                f["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(args, r#"{"to":"a@x.com"}"#);
    }

    #[test]
    fn done_passes_through_untouched() {
        let mut s = StreamShaper::new("m".into(), &ctx(None), Dialect::Chat);
        let out = s.process(b"data: [DONE]\n\n");
        assert_eq!(out, b"data: [DONE]\n\n");
    }
}
