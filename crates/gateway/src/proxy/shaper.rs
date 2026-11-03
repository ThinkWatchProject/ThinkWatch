//! The last step before bytes reach the client: put the caller's model
//! name back and paint their redacted values back in.
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
//! **Redacted values.** A whole response has its placeholders intact and
//! is restored in one pass (`redaction::restore_body`). A stream does not:
//! `<<TW_EM` can end one frame and `AIL_1>>` start the next, with frame
//! structure in between. Restoration happens per frame, on the text, the
//! tool arguments and the freeform tool input (Codex's `apply_patch`) of
//! whichever format this is, with one lane per content block or tool call —
//! thinkwatch-core's `FrameRestorer`, the same one the desktop gateway uses.
//! A value goes into text as it reads and into tool arguments, which are
//! JSON, escaped.
//!
//! **Answer text.** Content rules that check answers run next, on the
//! text with the caller's values back in it — what the client would read —
//! and before tool calls are inspected on the shaper's output. Each text
//! lane holds back only the tail that could still grow into a match, and
//! frames that repeat the whole text (Responses' `output_text.done`,
//! `output_item.done`, `response.completed`) are written as the client
//! received it — thinkwatch-core's `FrameFilter`. The rules found are
//! collected for the request's audit events ([`AnswerHits`]).
//!
//! **Usage.** A Chat stream is always sent upstream asking for its usage
//! chunk, or there would be nothing to bill. When the caller did not ask
//! for it, the shaper takes it back out: the trailing chunk that carries
//! only `usage`, and the `"usage": null` the upstream adds to every other
//! chunk once asked.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tw_dialect::frame::{self, Decoder, Frame};
use tw_dialect::ir::Dialect;
use tw_guard::content::answer::{AnswerHit, FrameFilter};
use tw_guard::redact::replace::Ledger;
use tw_guard::redact::sse::{FrameRestorer, Synth};

/// What the content rules found in a streamed answer so far, one entry per
/// rule. Shared with the stream's tail, which writes the audit events
/// whether or not the client stayed to the end.
pub type AnswerHits = Arc<Mutex<Vec<AnswerHit>>>;

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
    client: Dialect,
    restorer: Option<FrameRestorer>,
    filter: Option<FrameFilter>,
    hits: AnswerHits,
    hide_usage: bool,
}

impl StreamShaper {
    pub fn new(model: String, redaction: &Ledger, client: Dialect) -> Self {
        let mut shaper = Self {
            decoder: Decoder::default(),
            model,
            client,
            restorer: None,
            filter: None,
            hits: AnswerHits::default(),
            hide_usage: false,
        };
        shaper.restore_with(redaction);
        shaper
    }

    /// Restore with `ledger` from here on. Called once the upstream has
    /// answered, before any of its bytes: the hop that went out can have
    /// numbered a value the request's own ledger does not hold.
    pub fn restore_with(&mut self, ledger: &Ledger) {
        let restorer = FrameRestorer::new(ledger, self.client);
        self.restorer = (!restorer.is_noop()).then_some(restorer);
    }

    /// Filter the answer's text with the content rules that check answers.
    /// `None`: no rule does.
    pub fn filtering(mut self, filter: Option<FrameFilter>) -> Self {
        self.filter = filter;
        self
    }

    /// What the content rules found so far, for the stream's tail.
    pub fn answer_hits(&self) -> AnswerHits {
        self.hits.clone()
    }

    /// Take the usage the caller did not ask for out of a Chat stream.
    pub fn hiding_usage(mut self, hide: bool) -> Self {
        self.hide_usage = hide;
        self
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.decoder.feed(chunk);
        let out = self.write(frames).into_bytes();
        self.collect_hits();
        out
    }

    /// Frames the gateway writes itself (a refusal that ends the stream),
    /// with the caller's model name put in. Read on their own, apart from
    /// the stream's frames — the stream may be stopped mid-frame — and
    /// with nothing to restore.
    pub fn rename(&self, sse: &[u8]) -> Vec<u8> {
        let mut decoder = Decoder::default();
        let mut frames = decoder.feed(sse);
        frames.extend(decoder.flush());
        let mut out = String::new();
        for f in frames {
            match serde_json::from_str::<Value>(&f.data) {
                Ok(mut v) => {
                    set_model(&mut v, &self.model);
                    out.push_str(&match &f.event {
                        Some(e) => frame::named(e, &v),
                        None => frame::data(&v),
                    });
                }
                Err(_) => out.push_str(&raw(&f)),
            }
        }
        out.into_bytes()
    }

    /// The stream ended. Emits whatever the decoder was still holding, then
    /// any text held back waiting to be a placeholder.
    pub fn finish(&mut self) -> Vec<u8> {
        let frames = self.decoder.flush();
        let mut out = self.write(frames);
        self.drain(&mut out);
        self.collect_hits();
        out.into_bytes()
    }

    fn collect_hits(&mut self) {
        let Some(f) = self.filter.as_mut() else {
            return;
        };
        let found = f.take_hits();
        if found.is_empty() {
            return;
        }
        if let Ok(mut hits) = self.hits.lock() {
            crate::content_filter::merge_answer_hits(&mut hits, found);
        }
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
                self.filtered(s, out);
            }
        }
        if let Some(f) = self.filter.as_mut() {
            for s in f.frame(&mut v).before {
                self.synth(s, out);
            }
        }
        set_model(&mut v, &self.model);
        out.push_str(&match &f.event {
            Some(e) => frame::named(e, &v),
            None => frame::data(&v),
        });
    }

    /// Every tail still held back, restored values first, then the
    /// answer's text — both written before whatever ends the stream.
    fn drain(&mut self, out: &mut String) {
        if let Some(r) = self.restorer.as_mut() {
            for s in r.drain() {
                self.filtered(s, out);
            }
        }
        if let Some(f) = self.filter.as_mut() {
            for s in f.drain() {
                self.synth(s, out);
            }
        }
    }

    /// A frame the restorer wrote, through the answer filter like any
    /// frame of the stream.
    fn filtered(&mut self, mut s: Synth, out: &mut String) {
        if let Some(f) = self.filter.as_mut() {
            for before in f.frame(&mut s.data).before {
                self.synth(before, out);
            }
        }
        self.synth(s, out);
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

    /// The test requests are written as chat completions.
    const CHAT: crate::redaction::Shape = crate::redaction::Shape::Generate(Dialect::Chat);

    /// A ledger that issued `<<TW_EMAIL_1>>` for `a@x.com`, or nothing.
    fn ctx(email: Option<&str>) -> Ledger {
        let r = crate::redaction::Redaction::new(
            &tw_guard::policy::RedactPolicy {
                mode: tw_guard::policy::Mode::Enforce,
                enable: vec!["email".into()],
                ..Default::default()
            },
            &Default::default(),
        );
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": email.unwrap_or("")}
        ]})
        .to_string();
        let ledger = r.look(body.as_bytes(), CHAT).1;
        assert_eq!(ledger.len(), usize::from(email.is_some()));
        ledger
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
        let mut out = s.process(chat_chunk("mail <<TW_EM").as_bytes());
        out.extend(s.process(chat_chunk("AIL_1>> now").as_bytes()));
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
            ev(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"to <<TW_EMAIL_"}})).as_bytes(),
        );
        out.extend(s.process(
            ev(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1>>"}})).as_bytes(),
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
        // Text ending in an unclosed `<<` is not a placeholder: it goes out
        // verbatim, inside the block it belongs to, not after the block ends.
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Anthropic);
        let ev = |name: &str, d: Value| format!("event: {name}\ndata: {d}\n\n");
        let mut out = s.process(
            ev("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"literal <<"}})).as_bytes(),
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
        assert_eq!(text, "literal <<");
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
                serde_json::json!({"type":"response.output_text.done","text":"mail <<TW_EMAIL_1>>"})
            )
            .as_bytes(),
        );
        assert_eq!(frames(&out)[0]["text"], "mail a@x.com");
    }

    #[test]
    fn a_tool_calls_arguments_get_the_callers_values_back() {
        // The old shaper restored text only: a model asked to "email
        // a@x.com" called the tool with the placeholder as the address.
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Chat);
        let call = |args: &str| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"model":"up","choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":args}}
                ]},"finish_reason":null}]})
            )
        };
        let mut out = s.process(call(r#"{"to":"<<TW_EM"#).as_bytes());
        out.extend(s.process(call(r#"AIL_1>>"}"#).as_bytes()));
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

    const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\n-----END RSA PRIVATE KEY-----";

    /// A ledger that issued `<<TW_SECRET_1>>` for a private key, found in a
    /// request body where its line breaks are written `\n`.
    fn pem_ctx() -> Ledger {
        let r = crate::redaction::Redaction::new(
            &tw_guard::policy::RedactPolicy {
                mode: tw_guard::policy::Mode::Enforce,
                ..Default::default()
            },
            &Default::default(),
        );
        let body = serde_json::json!({"messages": [{"role": "user", "content": PEM}]}).to_string();
        let ledger = r.look(body.as_bytes(), CHAT).1;
        assert_eq!(ledger.len(), 1);
        ledger
    }

    #[test]
    fn a_private_key_in_streamed_text_comes_back_with_its_line_breaks() {
        let mut s = StreamShaper::new("m".into(), &pem_ctx(), Dialect::Anthropic);
        let ev = |t: &str| {
            format!(
                "event: content_block_delta\ndata: {}\n\n",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":t}})
            )
        };
        let mut out = s.process(ev("key: <<TW_SEC").as_bytes());
        out.extend(s.process(ev("RET_1>> done").as_bytes()));
        out.extend(s.finish());
        let text: String = frames(&out)
            .iter()
            .filter_map(|f| f["delta"]["text"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, format!("key: {PEM} done"));
    }

    #[test]
    fn a_private_key_in_streamed_tool_arguments_leaves_them_parseable() {
        let mut s = StreamShaper::new("m".into(), &pem_ctx(), Dialect::Chat);
        let call = |args: &str| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"model":"up","choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":args}}
                ]},"finish_reason":null}]})
            )
        };
        let mut out = s.process(call(r#"{"content":"<<TW_SECRET"#).as_bytes());
        out.extend(s.process(call(r#"_1>>"}"#).as_bytes()));
        out.extend(s.finish());
        let args: String = frames(&out)
            .iter()
            .filter_map(|f| {
                f["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        let args: Value = serde_json::from_str(&args).expect("the arguments parse");
        assert_eq!(args["content"], PEM);
    }

    #[test]
    fn a_freeform_tool_input_gets_a_split_placeholder_back() {
        // Codex's `apply_patch` streams its input as plain text, in its own
        // event; a placeholder cut across two of them is put back too.
        let mut s = StreamShaper::new("m".into(), &pem_ctx(), Dialect::Responses);
        let delta = |d: &str| {
            format!(
                "event: response.custom_tool_call_input.delta\ndata: {}\n\n",
                serde_json::json!({"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","output_index":1,"delta":d})
            )
        };
        let mut out = s.process(delta("*** Begin Patch\n+key = <<TW_").as_bytes());
        out.extend(s.process(delta("SECRET_1>>\n*** End Patch").as_bytes()));
        out.extend(s.finish());
        let input: String = frames(&out)
            .iter()
            .filter(|f| f["type"] == "response.custom_tool_call_input.delta")
            .filter_map(|f| f["delta"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            input,
            format!("*** Begin Patch\n+key = {PEM}\n*** End Patch")
        );
    }

    #[test]
    fn done_passes_through_untouched() {
        let mut s = StreamShaper::new("m".into(), &ctx(None), Dialect::Chat);
        let out = s.process(b"data: [DONE]\n\n");
        assert_eq!(out, b"data: [DONE]\n\n");
    }

    /// A content filter whose one rule checks answers for `pattern`.
    fn answer_filter(
        mode: tw_guard::policy::Mode,
        pattern: &str,
        action: tw_guard::policy::ContentAction,
        client: Dialect,
    ) -> Option<FrameFilter> {
        crate::content_filter::ContentFilter::new(&tw_guard::policy::ContentPolicy {
            mode,
            custom: vec![tw_guard::policy::CustomContentRule {
                name: "Codename".into(),
                pattern: pattern.into(),
                scope: vec![tw_guard::policy::ContentScope::Response],
                matching: tw_guard::policy::ContentMatch::Contains,
                case_sensitive: false,
                action,
                replacement: "***".into(),
                disabled: false,
            }],
            ..Default::default()
        })
        .stream(client)
    }

    fn chat_text(out: &[u8]) -> String {
        frames(out)
            .iter()
            .filter_map(|f| {
                f["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn a_match_split_across_frames_never_reaches_the_client() {
        use tw_guard::policy::{ContentAction, Mode};
        let mut s =
            StreamShaper::new("m".into(), &ctx(None), Dialect::Chat).filtering(answer_filter(
                Mode::Enforce,
                "Polaris",
                ContentAction::Replace,
                Dialect::Chat,
            ));
        let hits = s.answer_hits();
        let mut out = Vec::new();
        for piece in ["The Pol", "aris release ", "is near; POLA", "RIS"] {
            let bytes = s.process(chat_chunk(piece).as_bytes());
            // No frame that went out carries a piece of the codename.
            let sent = chat_text(&bytes).to_lowercase();
            assert!(!sent.contains("pol") || sent.contains("***"), "{sent}");
            out.extend(bytes);
        }
        out.extend(s.process(b"data: [DONE]\n\n"));
        out.extend(s.finish());
        assert_eq!(chat_text(&out), "The *** release is near; ***");
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("data: [DONE]\n\n"),
            "the held tail goes out before the terminator"
        );
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].count, 2);
        assert_eq!(hits[0].outcome, tw_guard::content::Outcome::Replaced);
    }

    #[test]
    fn only_the_tail_that_could_still_match_is_held_back() {
        use tw_guard::policy::{ContentAction, Mode};
        const TEXT: &str = "a long answer that goes on";
        // How many characters of TEXT reach the client at once, with one
        // rule looking for the seven characters of "Polaris".
        let sent_at_once = |action: ContentAction| {
            let mut s = StreamShaper::new("m".into(), &ctx(None), Dialect::Chat).filtering(
                answer_filter(Mode::Enforce, "Polaris", action, Dialect::Chat),
            );
            let out = s.process(chat_chunk(TEXT).as_bytes());
            let sent = chat_text(&out);
            let mut all = out;
            all.extend(s.finish());
            assert_eq!(
                chat_text(&all),
                TEXT,
                "{action:?}: all of it goes out in the end"
            );
            sent.chars().count()
        };
        let len = TEXT.chars().count();
        // A replacement with text: at most the keyword's length less one.
        assert!(sent_at_once(ContentAction::Replace) >= len - 6);
        // A deletion holds back as many again: text that comes together
        // where something was deleted is checked again.
        let strip = sent_at_once(ContentAction::Strip);
        assert!(strip >= len - 12, "{strip}");
        assert!(strip <= len - 6, "{strip}");
        // Recording changes nothing, so nothing waits.
        assert_eq!(sent_at_once(ContentAction::Record), len);
    }

    #[test]
    fn a_rule_sees_the_restored_value_and_the_whole_text_frames_agree() {
        use tw_guard::policy::{ContentAction, Mode};
        // The caller's address was a placeholder upstream; the rule matches
        // what the client would read.
        let mut s = StreamShaper::new("m".into(), &ctx(Some("a@x.com")), Dialect::Responses)
            .filtering(answer_filter(
                Mode::Enforce,
                "a@x.com",
                ContentAction::Replace,
                Dialect::Responses,
            ));
        let ev = |name: &str, d: Value| format!("event: {name}\ndata: {d}\n\n");
        let mut out = s.process(
            ev(
                "response.output_text.delta",
                serde_json::json!({"type": "response.output_text.delta", "item_id": "m1",
                    "output_index": 0, "content_index": 0, "delta": "mail <<TW_EMAIL_1>> today"}),
            )
            .as_bytes(),
        );
        out.extend(
            s.process(
                ev(
                    "response.output_text.done",
                    serde_json::json!({"type": "response.output_text.done", "item_id": "m1",
                        "output_index": 0, "content_index": 0, "text": "mail <<TW_EMAIL_1>> today"}),
                )
                .as_bytes(),
            ),
        );
        out.extend(s.finish());
        let fs = frames(&out);
        let deltas: String = fs
            .iter()
            .filter(|f| f["type"] == "response.output_text.delta")
            .filter_map(|f| f["delta"].as_str().map(str::to_string))
            .collect();
        assert_eq!(deltas, "mail *** today");
        let done = fs
            .iter()
            .find(|f| f["type"] == "response.output_text.done")
            .unwrap();
        assert_eq!(
            done["text"], "mail *** today",
            "the repeat agrees with what was sent"
        );
    }

    #[test]
    fn monitoring_passes_the_answer_on_unchanged_and_still_finds() {
        use tw_guard::policy::{ContentAction, Mode};
        let mut s =
            StreamShaper::new("m".into(), &ctx(None), Dialect::Anthropic).filtering(answer_filter(
                Mode::Observe,
                "Polaris",
                ContentAction::Strip,
                Dialect::Anthropic,
            ));
        let hits = s.answer_hits();
        let ev = |t: &str| {
            format!(
                "event: content_block_delta\ndata: {}\n\n",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":t}})
            )
        };
        let first = s.process(ev("Pola").as_bytes());
        // Nothing is held back when nothing would be changed.
        assert_eq!(frames(&first)[0]["delta"]["text"], "Pola");
        let mut out = first;
        out.extend(s.process(ev("ris").as_bytes()));
        out.extend(
            s.process(
                format!(
                    "event: content_block_stop\ndata: {}\n\n",
                    serde_json::json!({"type":"content_block_stop","index":0})
                )
                .as_bytes(),
            ),
        );
        out.extend(s.finish());
        let text: String = frames(&out)
            .iter()
            .filter_map(|f| f["delta"]["text"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, "Polaris");
        let hits = hits.lock().unwrap();
        assert_eq!(hits[0].outcome, tw_guard::content::Outcome::Recorded);
    }
}
