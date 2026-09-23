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
//! in one pass (`pii_redactor::restore_body`). A stream does not: `{{EMA`
//! can end one frame and `IL_1}}` start the next, with frame structure in
//! between. Restoration happens per frame, on the text and the tool
//! arguments of whichever format this is, with one lane per content block
//! or tool call — thinkwatch-core's `FrameRestorer`, the same one the
//! desktop gateway uses.

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
    restorer: Option<FrameRestorer>,
}

impl StreamShaper {
    pub fn new(model: String, redaction: &Ledger, client: Dialect) -> Self {
        let restorer = FrameRestorer::new(redaction, client);
        Self {
            decoder: Decoder::default(),
            model,
            restorer: (!restorer.is_noop()).then_some(restorer),
        }
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
