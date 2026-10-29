//! Content filter: what the caller sends, and the text of the answers,
//! checked against the rules.
//!
//! The rules, the engine and the verdict are thinkwatch-core's
//! (`tw_guard::content`), shared with the desktop gateway:
//!
//! - **Which text** — each rule says (`scope`). On the request: the
//!   caller's messages and the tool results inside them, the place an
//!   injected instruction most often rides in: a page a tool fetched, a
//!   file it read; not the system prompt (the operator's) and not the
//!   model's earlier turns. On the answer: the text the client is shown,
//!   and the input of a freeform tool such as Codex's `apply_patch`; not
//!   tool calls (dangerous commands look at those) and not thinking, whose
//!   signature a changed word would break. Built-in rules check requests.
//! - **How a rule matches** — a keyword, a regex (size-bounded), both
//!   ignoring case unless the rule says otherwise, or code points
//!   (`U+200B`, `U+E0000–U+E007F`, requests only): the built-in
//!   hidden-character rules are the last kind, characters an editor does
//!   not show and a model still reads.
//! - **What a hit does in enforce mode** — each rule its own: refuse the
//!   request (requests only), delete the matched text, replace it with the
//!   rule's text (`***` by default; not restored), or record only. Observe
//!   mode records every hit and changes nothing.
//!
//! A request with text deleted or replaced goes on as the changed one: the
//! gateway decodes it again, and redaction, forwarding and the audit row
//! all see what was actually sent.
//!
//! **An answer is never held back whole.** It is filtered on its way to
//! the client, in the client's format, after redacted values are put back
//! and before tool calls are inspected: a stream frame by frame
//! (`proxy::shaper`), each text lane holding back only the tail that could
//! still grow into a match, and a whole answer in one pass ([`whole`]).
//! What a rule deletes or replaces never reaches the client.

use std::sync::Arc;

use think_watch_common::audit::AuditLogger;
use tw_dialect::ir::Dialect;
use tw_guard::content::answer::{Answer, AnswerHit, FrameFilter};
use tw_guard::content::{self, Match, Outcome, Rules, ScreenHit, Screening};
use tw_guard::policy::{ContentPolicy, Mode};

use crate::error::GatewayError;
use crate::guards::Caller;
use crate::redaction::Redaction;

/// The content filter as configured: a mode and the rules.
#[derive(Debug, Clone)]
pub struct ContentFilter {
    pub mode: Mode,
    pub rules: Arc<Rules>,
    /// The rules that check answers, compiled for them. `None` when off or
    /// when no rule checks answers: then answers are not touched at all.
    pub answer: Option<Arc<Answer>>,
}

impl ContentFilter {
    pub fn new(policy: &ContentPolicy) -> Self {
        let rules = think_watch_common::guard_policy::content_rules(policy);
        let answer = if policy.mode.detects() {
            Answer::build(&rules)
        } else {
            None
        };
        Self {
            mode: policy.mode,
            rules: Arc::new(rules),
            answer,
        }
    }

    /// A filter for one streamed answer, frame by frame in the client's
    /// format. `None` when no rule checks answers.
    pub fn stream(&self, client: Dialect) -> Option<FrameFilter> {
        let answer = self.answer.as_ref()?;
        Some(FrameFilter::new(answer.clone(), self.mode.acts(), client))
    }

    /// Filter a whole answer in the client's format: the answer as the
    /// client receives it, and every rule that matched. The answer comes
    /// back unchanged (and nothing is found) when no rule checks answers.
    pub fn whole(&self, client: Dialect, body: Vec<u8>) -> (Vec<u8>, Vec<AnswerHit>) {
        let Some(answer) = self.answer.as_ref() else {
            return (body, Vec::new());
        };
        let (changed, hits) = content::answer::whole(answer, self.mode.acts(), client, &body);
        (changed.unwrap_or(body), hits)
    }

    /// Check a request as the caller sent it, `dialect` being its format.
    /// Every rule that fires is in the result with what became of it; a
    /// refusal names the hit that decided it; a request with text
    /// stripped comes back as its new body.
    pub fn screen(&self, dialect: Dialect, body: &[u8]) -> Screening {
        content::screen(self.mode, &self.rules, dialect, body)
    }
}

/// What the caller is told when a rule refuses the request: the rule,
/// and where and what it matched, so they can fix the prompt. The
/// matched text is masked with the redaction rules — the message also
/// lands in the request's log row.
pub fn refusal(hit: &ScreenHit, mask: &Redaction) -> GatewayError {
    let h = &hit.hit;
    let place = if h.in_tool_result {
        "a tool result"
    } else {
        "the message"
    };
    // A code-point rule matched characters that cannot be shown, only
    // counted.
    let message = if h.matching == Match::Codepoints {
        format!(
            "Request blocked by content filter: rule '{}' found {} invisible character{} in {place}",
            h.name,
            h.count,
            if h.count == 1 { "" } else { "s" },
        )
    } else {
        format!(
            "Request blocked by content filter: rule '{}' matched in {place}: \"{}\"",
            h.name,
            mask.mask(&h.snippet),
        )
    };
    GatewayError::PolicyBlocked(message)
}

/// At most this many content filter events per request and side: the
/// refusing hit first, then what was replaced or deleted, then what was
/// only recorded.
pub const RULE_EVENTS_MAX: usize = 20;

fn weight(o: Outcome) -> u8 {
    match o {
        Outcome::Blocked => 0,
        Outcome::Replaced => 1,
        Outcome::Stripped => 2,
        Outcome::Recorded => 3,
    }
}

/// The audit action of an outcome.
pub fn action_of(o: Outcome) -> &'static str {
    match o {
        Outcome::Recorded => "gateway.content_flagged",
        Outcome::Stripped => "gateway.content_stripped",
        Outcome::Replaced => "gateway.content_replaced",
        Outcome::Blocked => "gateway.content_blocked",
    }
}

/// The hits of a screening in the order their events are written, the
/// ones that changed the request first.
pub fn by_weight(screening: &Screening) -> Vec<&ScreenHit> {
    let mut hits: Vec<&ScreenHit> = screening.hits.iter().collect();
    hits.sort_by_key(|h| weight(h.outcome));
    hits
}

/// Record the hits of a screening: an audit event per rule
/// (`gateway.content_flagged`, `gateway.content_stripped`,
/// `gateway.content_replaced` or `gateway.content_blocked`, with
/// `direction: request`), at most [`RULE_EVENTS_MAX`] of them, a counter,
/// and a log line.
///
/// **No text of the request is written**, not even a masked excerpt: the
/// matched text is part of the request body, and the body is for those who
/// may read bodies (`logs:read_bodies`), not for everyone who reads the
/// audit log. An event says which rule, what became of the request, how
/// many matches and whether they were in a tool result.
pub fn record(audit: &AuditLogger, caller: &Caller, mode: Mode, screening: &Screening) {
    let hits = by_weight(screening);
    if hits.len() > RULE_EVENTS_MAX {
        tracing::warn!(
            trace_id = %caller.trace_id,
            rules = hits.len(),
            written = RULE_EVENTS_MAX,
            "more content rules matched one request than are written to the audit log"
        );
    }
    for (i, s) in hits.iter().enumerate() {
        let h = &s.hit;
        tracing::info!(
            trace_id = %caller.trace_id,
            rule = %h.rule,
            outcome = s.outcome.slug(),
            in_tool_result = h.in_tool_result,
            count = h.count,
            "content rule matched in the request (text withheld)"
        );
        metrics::counter!(
            "gateway_content_matched_total",
            "outcome" => s.outcome.slug(),
            "custom" => if h.custom { "true" } else { "false" },
            "direction" => "request",
        )
        .increment(1);
        if i >= RULE_EVENTS_MAX {
            continue;
        }
        audit.log(
            caller
                .audit(action_of(s.outcome))
                .detail(serde_json::json!({
                    "trace_id": caller.trace_id,
                    "model": caller.model,
                    "rule": h.rule,
                    "rule_name": h.name,
                    "custom": h.custom,
                    "action": h.action.slug(),
                    "outcome": s.outcome.slug(),
                    "mode": mode.slug(),
                    "direction": "request",
                    "in_tool_result": h.in_tool_result,
                    "count": h.count,
                    "rules_in_request": hits.len(),
                })),
        );
    }
}

/// Fold `more` into `into`: one entry per rule, counts added up. A stream's
/// hits are taken as it goes, so one rule can come back more than once.
pub fn merge_answer_hits(into: &mut Vec<AnswerHit>, more: Vec<AnswerHit>) {
    for h in more {
        match into
            .iter_mut()
            .find(|x| x.rule == h.rule && x.custom == h.custom)
        {
            Some(x) => x.count += h.count,
            None => into.push(h),
        }
    }
}

/// Record what the rules found in an answer: an audit event per rule
/// (`gateway.content_flagged`, `gateway.content_stripped` or
/// `gateway.content_replaced`, with `direction: response`), at most
/// [`RULE_EVENTS_MAX`] of them, a counter, and a log line.
///
/// Like the request's events, **none carries the answer's text**: the
/// event names the rule, what became of the matches and how many there
/// were. The answer as the client received it is in the request's log row,
/// for those who may read bodies.
pub fn record_answer(
    audit: &AuditLogger,
    caller: &Caller,
    provider: &str,
    mode: Mode,
    hits: &[AnswerHit],
) {
    if hits.is_empty() {
        return;
    }
    let mut ordered: Vec<&AnswerHit> = hits.iter().collect();
    ordered.sort_by_key(|h| weight(h.outcome));
    if ordered.len() > RULE_EVENTS_MAX {
        tracing::warn!(
            trace_id = %caller.trace_id,
            rules = ordered.len(),
            written = RULE_EVENTS_MAX,
            "more content rules matched one answer than are written to the audit log"
        );
    }
    for (i, h) in ordered.iter().enumerate() {
        tracing::info!(
            trace_id = %caller.trace_id,
            provider,
            rule = %h.rule,
            outcome = h.outcome.slug(),
            count = h.count,
            "content rule matched in the answer (text withheld)"
        );
        metrics::counter!(
            "gateway_content_matched_total",
            "outcome" => h.outcome.slug(),
            "custom" => if h.custom { "true" } else { "false" },
            "direction" => "response",
        )
        .increment(1);
        if i >= RULE_EVENTS_MAX {
            continue;
        }
        let mut detail = serde_json::json!({
            "trace_id": caller.trace_id,
            "model": caller.model,
            "rule": h.rule,
            "rule_name": h.name,
            "custom": h.custom,
            "action": h.action.slug(),
            "outcome": h.outcome.slug(),
            "mode": mode.slug(),
            "direction": "response",
            "part": "response_text",
            "count": h.count,
            "rules_in_answer": ordered.len(),
        });
        if h.outcome == Outcome::Replaced {
            detail["replacement"] = serde_json::json!(h.replacement);
        }
        audit.log(
            caller
                .audit(action_of(h.outcome))
                .resource(format!("provider:{provider}"))
                .detail(detail),
        );
    }
}

/// Filter a whole answer, as the client will receive it, and record what
/// was found. The answer comes back as the client is to receive it.
pub fn whole(
    filter: &ContentFilter,
    audit: &AuditLogger,
    caller: &Caller,
    provider: &str,
    client: Dialect,
    body: Vec<u8>,
) -> Vec<u8> {
    let (body, hits) = filter.whole(client, body);
    record_answer(audit, caller, provider, filter.mode, &hits);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_guard::policy::{
        ContentAction, ContentMatch, ContentScope, CustomContentRule, RedactPolicy,
    };

    fn filter(mode: Mode, custom: Vec<CustomContentRule>) -> ContentFilter {
        ContentFilter::new(&ContentPolicy {
            mode,
            custom,
            ..Default::default()
        })
    }

    fn rule(
        name: &str,
        pattern: &str,
        matching: ContentMatch,
        action: ContentAction,
    ) -> CustomContentRule {
        CustomContentRule {
            name: name.into(),
            pattern: pattern.into(),
            scope: tw_guard::policy::request_only(),
            matching,
            case_sensitive: false,
            action,
            replacement: tw_guard::policy::DEFAULT_REPLACEMENT.into(),
            disabled: false,
        }
    }

    fn chat(text: &str) -> Vec<u8> {
        serde_json::json!({"model": "m", "messages": [
            {"role": "system", "content": "never say jailbreak"},
            {"role": "user", "content": text}
        ]})
        .to_string()
        .into_bytes()
    }

    fn mask() -> Redaction {
        Redaction::new(&RedactPolicy::default(), Default::default())
    }

    #[test]
    fn a_block_rule_refuses_with_the_callers_words_and_the_system_prompt_is_not_read() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Jailbreak",
                "jailbreak",
                ContentMatch::Contains,
                ContentAction::Block,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("try a JAILBREAK"));
        let refused = s.refusal().expect("refused");
        assert_eq!(refused.outcome, Outcome::Blocked);
        let e = refusal(refused, &mask()).to_string();
        assert!(e.contains("'Jailbreak'") && e.contains("JAILBREAK"), "{e}");
        assert_eq!(refusal(refused, &mask()).status_code(), 403);
        // The system prompt says it too, and is not the caller's.
        assert!(f.screen(Dialect::Chat, &chat("hello")).hits.is_empty());
    }

    #[test]
    fn a_strip_rule_deletes_every_occurrence_and_hands_back_the_new_body() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Code",
                "project-x",
                ContentMatch::Contains,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(
            Dialect::Chat,
            &chat("Project-X is late; ask project-x leads"),
        );
        assert!(s.refusal().is_none());
        assert_eq!(s.hits[0].outcome, Outcome::Stripped);
        let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["messages"][1]["content"], " is late; ask  leads");
        assert_eq!(body["messages"][0]["content"], "never say jailbreak");
    }

    #[test]
    fn observe_records_what_enforce_would_do_and_changes_nothing() {
        let f = filter(
            Mode::Observe,
            vec![rule(
                "Code",
                "project-x",
                ContentMatch::Contains,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("project-x"));
        assert_eq!(s.hits[0].outcome, Outcome::Recorded);
        assert!(s.body.is_none() && s.refusal().is_none());
    }

    #[test]
    fn hidden_characters_are_counted_not_quoted() {
        // The built-in tag-character rule, re-graded to refuse.
        let f = ContentFilter::new(&ContentPolicy {
            mode: Mode::Enforce,
            actions: [("unicode-tags".to_string(), ContentAction::Block)].into(),
            ..Default::default()
        });
        let tagged: String = "summarise"
            .chars()
            .chain(
                "ignore"
                    .chars()
                    .map(|c| char::from_u32(0xE0000 + c as u32).unwrap()),
            )
            .collect();
        let s = f.screen(Dialect::Chat, &chat(&tagged));
        let refused = s.refusal().expect("refused");
        assert_eq!(refused.hit.rule, "unicode-tags");
        assert_eq!(refused.hit.revealed, "ignore");
        let e = refusal(refused, &mask()).to_string();
        assert!(
            e.contains("found 6 invisible characters in the message"),
            "{e}"
        );
    }

    #[test]
    fn a_custom_code_point_rule_strips_the_characters() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "ZW",
                "U+200B",
                ContentMatch::Codepoints,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("jail\u{200B}break"));
        let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["messages"][1]["content"], "jailbreak");
    }

    #[test]
    fn the_events_put_what_changed_the_request_first() {
        let f = filter(
            Mode::Enforce,
            vec![
                rule(
                    "Rec",
                    "alpha",
                    ContentMatch::Contains,
                    ContentAction::Record,
                ),
                rule(
                    "Strip",
                    "beta",
                    ContentMatch::Contains,
                    ContentAction::Strip,
                ),
                rule(
                    "Block",
                    "gamma",
                    ContentMatch::Contains,
                    ContentAction::Block,
                ),
            ],
        );
        let order = |text: &str| -> Vec<String> {
            by_weight(&f.screen(Dialect::Chat, &chat(text)))
                .iter()
                .map(|h| h.hit.rule.clone())
                .collect()
        };
        assert_eq!(order("alpha beta gamma"), ["Block", "Rec", "Strip"]);
        assert_eq!(order("alpha beta"), ["Strip", "Rec"]);
    }

    #[test]
    fn a_refusal_quoting_a_credential_masks_it() {
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Keys",
                "my key",
                ContentMatch::Contains,
                ContentAction::Block,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat(&format!("my key {key}")));
        let e = refusal(s.refusal().unwrap(), &mask()).to_string();
        assert!(!e.contains(key), "{e}");
        assert!(e.contains("sk-an…"), "{e}");
    }

    fn answer_rule(
        pattern: &str,
        action: ContentAction,
        scope: Vec<ContentScope>,
    ) -> CustomContentRule {
        CustomContentRule {
            scope,
            ..rule("Codename", pattern, ContentMatch::Contains, action)
        }
    }

    #[test]
    fn a_replacing_rule_writes_its_text_into_the_request() {
        let f = filter(
            Mode::Enforce,
            vec![CustomContentRule {
                replacement: "[hidden]".into(),
                ..rule(
                    "Code",
                    "project-x",
                    ContentMatch::Contains,
                    ContentAction::Replace,
                )
            }],
        );
        let s = f.screen(Dialect::Chat, &chat("Project-X ships; ask project-x"));
        assert!(s.refusal().is_none());
        assert_eq!(s.hits[0].outcome, Outcome::Replaced);
        assert_eq!(action_of(s.hits[0].outcome), "gateway.content_replaced");
        let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            "[hidden] ships; ask [hidden]"
        );
        // The replacement goes before what was only recorded, after a refusal.
        assert!(weight(Outcome::Blocked) < weight(Outcome::Replaced));
        assert!(weight(Outcome::Replaced) < weight(Outcome::Stripped));
    }

    #[test]
    fn a_request_rule_leaves_answers_alone_and_an_answer_rule_leaves_requests_alone() {
        let f = filter(
            Mode::Enforce,
            vec![answer_rule(
                "Polaris",
                ContentAction::Strip,
                vec![ContentScope::Response],
            )],
        );
        assert!(f.screen(Dialect::Chat, &chat("Polaris")).hits.is_empty());
        let answer = serde_json::json!({"choices": [{"index": 0,
            "message": {"role": "assistant", "content": "Polaris ships"}}]})
        .to_string()
        .into_bytes();
        let (out, hits) = f.whole(Dialect::Chat, answer.clone());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], " ships");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].outcome, Outcome::Stripped);

        let request_only = filter(
            Mode::Enforce,
            vec![rule(
                "Codename",
                "Polaris",
                ContentMatch::Contains,
                ContentAction::Strip,
            )],
        );
        assert!(request_only.answer.is_none() && request_only.stream(Dialect::Chat).is_none());
        let (out, hits) = request_only.whole(Dialect::Chat, answer.clone());
        assert_eq!(out, answer, "untouched, byte for byte");
        assert!(hits.is_empty());
    }

    #[test]
    fn a_whole_answer_is_filtered_in_each_format_and_tool_calls_are_not() {
        let f = filter(
            Mode::Enforce,
            vec![CustomContentRule {
                replacement: "***".into(),
                ..answer_rule(
                    "Polaris",
                    ContentAction::Replace,
                    vec![ContentScope::Request, ContentScope::Response],
                )
            }],
        );
        let anthropic = serde_json::json!({"type": "message", "role": "assistant", "content": [
            {"type": "thinking", "thinking": "Polaris", "signature": "sig"},
            {"type": "text", "text": "About POLARIS: soon."},
            {"type": "tool_use", "id": "t1", "name": "note", "input": {"text": "Polaris"}}
        ]});
        let (out, hits) = f.whole(Dialect::Anthropic, anthropic.to_string().into_bytes());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["content"][1]["text"], "About ***: soon.");
        assert_eq!(
            v["content"][0]["thinking"], "Polaris",
            "thinking is not touched"
        );
        assert_eq!(
            v["content"][2]["input"]["text"], "Polaris",
            "tool calls are not touched"
        );
        assert_eq!((hits[0].outcome, hits[0].count), (Outcome::Replaced, 1));

        let responses = serde_json::json!({"id": "r", "object": "response", "output": [
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "Polaris and polaris"}
            ]}
        ]});
        let (out, hits) = f.whole(Dialect::Responses, responses.to_string().into_bytes());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["output"][0]["content"][0]["text"], "*** and ***");
        assert_eq!(hits[0].count, 2);
    }

    #[test]
    fn observe_finds_the_same_in_answers_and_changes_nothing() {
        let f = filter(
            Mode::Observe,
            vec![answer_rule(
                "Polaris",
                ContentAction::Strip,
                vec![ContentScope::Response],
            )],
        );
        let answer = serde_json::json!({"choices": [{"index": 0,
            "message": {"role": "assistant", "content": "Polaris ships"}}]})
        .to_string()
        .into_bytes();
        let (out, hits) = f.whole(Dialect::Chat, answer.clone());
        assert_eq!(out, answer);
        assert_eq!(hits[0].outcome, Outcome::Recorded);
        assert_eq!(action_of(hits[0].outcome), "gateway.content_flagged");

        let off = filter(
            Mode::Off,
            vec![answer_rule(
                "Polaris",
                ContentAction::Strip,
                vec![ContentScope::Response],
            )],
        );
        assert!(off.answer.is_none());
    }

    #[test]
    fn hits_taken_as_a_stream_goes_are_folded_into_one_per_rule() {
        let f = filter(
            Mode::Enforce,
            vec![
                answer_rule(
                    "Polaris",
                    ContentAction::Strip,
                    vec![ContentScope::Response],
                ),
                CustomContentRule {
                    name: "Other".into(),
                    ..answer_rule("Vega", ContentAction::Record, vec![ContentScope::Response])
                },
            ],
        );
        let one = |text: &str| {
            let body = serde_json::json!({"choices": [{"index": 0,
                "message": {"role": "assistant", "content": text}}]})
            .to_string()
            .into_bytes();
            f.whole(Dialect::Chat, body).1
        };
        let mut all = Vec::new();
        merge_answer_hits(&mut all, one("Polaris Polaris"));
        merge_answer_hits(&mut all, one("Polaris Vega"));
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].rule.as_str(), all[0].count), ("Codename", 3));
        assert_eq!((all[1].rule.as_str(), all[1].count), ("Other", 1));
    }
}
