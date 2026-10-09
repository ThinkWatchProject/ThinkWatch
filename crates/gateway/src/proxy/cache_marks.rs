//! Prompt-cache breakpoints the conversion added, when an upstream refuses
//! them.
//!
//! A request converted for an Anthropic-format upstream, or for a Claude
//! model on Bedrock, from a caller that marked no breakpoints gets them
//! from the conversion layer (`tw_dialect`'s `auto_cache`). Not every
//! upstream takes them: an Anthropic-compatible endpoint that is not
//! Anthropic's may not know `cache_control`, and a Bedrock model AWS
//! stops listing refuses `cachePoint`. Either answers `400` and the whole
//! request fails.
//!
//! So the request goes to the same upstream once more without them (see
//! `generate::send`), and that upstream is remembered as refusing them for
//! that model: later requests leave them out before they are sent, instead
//! of being refused first every time. The upstream itself is fine, so it
//! is not failed over and its health is not touched.
//!
//! **Only breakpoints the conversion added are taken out.** Ones the caller
//! marked are its own decision; they go out as written, and a refusal of
//! them goes back to the caller like any other. The desktop gateway does
//! the same (`tw_gateway::cache_marks` in thinkwatch-core).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::GatewayError;

/// At most this many models remembered per upstream. A full table drops
/// the one noted first: forgetting costs one more refusal and resend.
const MAX: usize = 1024;

/// The models one upstream refused the added breakpoints for.
///
/// In memory only, per instance, and rebuilt with the router: whatever
/// is forgotten costs one more refusal and resend.
#[derive(Default)]
pub struct Refused {
    /// Model → when it was noted, as a running count.
    by: Mutex<(HashMap<String, u64>, u64)>,
}

impl Refused {
    /// This upstream refused the added breakpoints for `model`.
    pub fn note(&self, model: &str) {
        let mut guard = self.by.lock().unwrap_or_else(|p| p.into_inner());
        let (by, clock) = &mut *guard;
        *clock += 1;
        if by.len() >= MAX
            && !by.contains_key(model)
            && let Some(oldest) = by.iter().min_by_key(|(_, at)| **at).map(|(m, _)| m.clone())
        {
            by.remove(&oldest);
        }
        by.insert(model.to_string(), *clock);
    }

    /// Has this upstream refused the added breakpoints for `model`?
    pub fn refused(&self, model: &str) -> bool {
        let guard = self.by.lock().unwrap_or_else(|p| p.into_inner());
        guard.0.contains_key(model)
    }
}

/// Is this the upstream refusing prompt-cache breakpoints: a `400` whose
/// wording names `cache_control`, `cachePoint` or prompt caching?
///
/// The wording, not the error's structure: compatible endpoints say
/// "messages.0.content.0.cache_control: Extra inputs are not permitted" or
/// "unknown field `cache_control`", Bedrock a `ValidationException` that
/// mentions `cachePoint` or prompt caching, each in a field of its own.
/// `label` is the upstream's name, which the error's message starts with.
pub fn is_refusal(err: &GatewayError, label: &str) -> bool {
    let GatewayError::ProviderHttpError {
        status: 400,
        message,
    } = err
    else {
        return false;
    };
    let said = message
        .strip_prefix(label)
        .and_then(|m| m.strip_prefix(": "))
        .unwrap_or(message)
        .to_ascii_lowercase();
    [
        "cache_control",
        "cachepoint",
        "cache_point",
        "cache point",
        "prompt caching",
        "prompt_caching",
    ]
    .iter()
    .any(|w| said.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, said: &str) -> GatewayError {
        GatewayError::ProviderHttpError {
            status,
            message: format!("relay: {said}"),
        }
    }

    #[test]
    fn what_upstreams_say_when_they_do_not_take_breakpoints() {
        for said in [
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.0.cache_control: Extra inputs are not permitted"}}"#,
            r#"{"error":{"message":"unknown field `cache_control`, expected one of `type`, `text`","code":"invalid_request"}}"#,
            r#"{"message":"The model returned the following errors: cachePoint is not supported for this model."}"#,
            r#"{"message":"Prompt caching is not supported for this model"}"#,
        ] {
            assert!(is_refusal(&http(400, said), "relay"), "{said}");
        }
        for (status, said) in [
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: Field required"}}"#,
            ),
            (
                400,
                r#"{"message":"Malformed input request: #/messages/0/content: expected type: JSONArray"}"#,
            ),
            // Not a refusal of the request: the upstream failing.
            (500, r#"{"message":"cache_control backend unavailable"}"#),
        ] {
            assert!(!is_refusal(&http(status, said), "relay"), "{said}");
        }
        // The upstream's own name is not what it said.
        assert!(!is_refusal(
            &GatewayError::ProviderHttpError {
                status: 400,
                message: "prompt caching proxy: max_tokens: Field required".into(),
            },
            "prompt caching proxy"
        ));
    }

    #[test]
    fn the_memory_is_per_model_and_stays_bounded() {
        let r = Refused::default();
        r.note("claude-opus-4-7");
        assert!(r.refused("claude-opus-4-7"));
        assert!(!r.refused("claude-sonnet-4-5"));
        for i in 0..MAX + 10 {
            r.note(&format!("m{i}"));
        }
        assert_eq!(r.by.lock().unwrap().0.len(), MAX);
        // The one noted first goes first.
        assert!(!r.refused("claude-opus-4-7"));
        assert!(r.refused(&format!("m{}", MAX + 9)));
    }
}
