//! Invisible characters in what the caller sends.
//!
//! Unicode tag characters (`U+E0000`–`U+E007F`) render as nothing in
//! almost every editor and still reach the model's token stream — a whole
//! instruction can ride along invisibly ("ASCII smuggling"). Bidirectional
//! overrides make text on screen read in a different order than the
//! characters really are. Neither has a legitimate use in a prompt, and
//! both show up where the caller did not write them: in a web page or a
//! file a tool fetched, handed back as a tool result.
//!
//! Detection is thinkwatch-core's (`tw_guard::hidden`), the scanner the
//! desktop gateway runs over client config files. Only the two kinds that
//! `tw_guard::hidden::Kind::smuggles` names are flagged here: zero-width joiners build
//! emoji, a zero-width non-joiner is ordinary Persian, and Cyrillic is
//! ordinary Russian.
//!
//! Scanned: the caller's messages and the tool results inside them.
//! Not scanned: the system prompt (the operator's) and the model's own
//! turns.

use serde::{Deserialize, Serialize};
use think_watch_common::dynamic_config::DynamicConfig;
use tw_dialect::ir::{Part, Request, Role};
use tw_guard::hidden;

/// What a hit does. Same words as the content filter's actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Off,
    /// Record it in the application log only.
    Log,
    /// Let the request through and write an audit event. The default:
    /// nothing breaks, and an operator sees it happening.
    #[default]
    Warn,
    /// Refuse the request with 403.
    Block,
}

/// `security.hidden_text`. Missing or unreadable means the default.
pub async fn action(dc: &DynamicConfig) -> Action {
    dc.get("security.hidden_text")
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// One kind of hidden character, where it was found and how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    /// `tag` or `bidi`
    pub kind: &'static str,
    /// Inside a tool result rather than text the caller typed.
    pub in_tool_result: bool,
    pub count: usize,
    /// The first code point seen, as `U+E0049`.
    pub example: String,
}

/// Scan the caller's messages, tool results included.
pub fn scan(request: &Request) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for m in request.messages.iter().filter(|m| m.role == Role::User) {
        scan_parts(&m.parts, false, &mut out);
    }
    out
}

fn scan_parts(parts: &[Part], in_tool_result: bool, out: &mut Vec<Found>) {
    for p in parts {
        match p {
            Part::Text(s) => {
                for h in hidden::scan(s).into_iter().filter(|h| h.kind.smuggles()) {
                    let kind = h.kind.slug();
                    match out
                        .iter_mut()
                        .find(|f| f.kind == kind && f.in_tool_result == in_tool_result)
                    {
                        Some(f) => f.count += 1,
                        None => out.push(Found {
                            kind,
                            in_tool_result,
                            count: 1,
                            example: h.codepoint,
                        }),
                    }
                }
            }
            Part::ToolResult(r) => scan_parts(&r.content, true, out),
            Part::Image(_) | Part::File { .. } | Part::Thinking(_) | Part::ToolCall(_) => {}
        }
    }
}

/// What the caller is told when the request is refused.
pub fn refusal(found: &[Found]) -> crate::error::GatewayError {
    let place = if found.iter().any(|f| f.in_tool_result) {
        "a tool result"
    } else {
        "the message"
    };
    crate::error::GatewayError::PolicyBlocked(format!(
        "{place} contains invisible characters that can hide instructions from a reader ({})",
        found.iter().map(|f| f.kind).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_dialect::ir::{Message, ToolResult};

    fn user(parts: Vec<Part>) -> Request {
        Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                parts,
            }],
            ..Default::default()
        }
    }

    /// "ignore" written in tag characters
    fn smuggled() -> String {
        "summarise this"
            .chars()
            .chain(
                "ignore"
                    .chars()
                    .map(|c| char::from_u32(0xE0000 + c as u32).unwrap()),
            )
            .collect()
    }

    #[test]
    fn tag_characters_in_a_tool_result_are_found() {
        let r = user(vec![Part::ToolResult(ToolResult {
            id: "t".into(),
            content: vec![Part::Text(smuggled())],
            is_error: false,
        })]);
        let found = scan(&r);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, "tag");
        assert!(found[0].in_tool_result);
        assert_eq!(found[0].count, 6);
        assert!(refusal(&found).to_string().contains("tool result"));
    }

    #[test]
    fn a_bidi_override_in_typed_text_is_found() {
        let found = scan(&user(vec![Part::Text("abc\u{202E}fed".into())]));
        assert_eq!(found[0].kind, "bidi");
        assert!(!found[0].in_tool_result);
    }

    #[test]
    fn ordinary_text_in_any_script_is_left_alone() {
        for s in [
            "👨\u{200D}👩\u{200D}👧 family",
            "Привет, как дела?",
            "می\u{200C}خواهم",
            "مرحبا بالعالم",
            "π ≈ 3.14",
        ] {
            assert!(scan(&user(vec![Part::Text(s.into())])).is_empty(), "{s}");
        }
    }

    #[test]
    fn the_system_prompt_and_the_models_turns_are_not_scanned() {
        let mut r = user(vec![Part::Text("hi".into())]);
        r.system = vec![smuggled()];
        r.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::Text(smuggled())],
        });
        assert!(scan(&r).is_empty());
    }

    #[test]
    fn the_setting_reads_the_content_filters_words() {
        for (s, a) in [
            ("off", Action::Off),
            ("log", Action::Log),
            ("warn", Action::Warn),
            ("block", Action::Block),
        ] {
            assert_eq!(serde_json::from_value::<Action>(s.into()).unwrap(), a);
        }
        assert_eq!(Action::default(), Action::Warn);
    }
}
