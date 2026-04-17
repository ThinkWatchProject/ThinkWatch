use crate::providers::traits::{ChatCompletionResponse, ChatMessage};
use rand::RngExt;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Serializable PII pattern for storage in system_settings.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PiiPatternConfig {
    pub name: String,
    pub regex: String,
    pub placeholder_prefix: String,
}

/// Detects and replaces PII in user messages before sending to upstream LLMs,
/// then restores original values in the response.
#[derive(Clone)]
pub struct PiiRedactor {
    patterns: Vec<PiiPattern>,
}

#[derive(Clone)]
struct PiiPattern {
    name: String,
    regex: Regex,
    placeholder_prefix: String,
}

/// Holds the mapping from placeholders back to original PII values.
pub struct RedactionContext {
    /// Maps placeholder (e.g. `{{EMAIL_a3f1_1}}`) to original value.
    pub replacements: HashMap<String, String>,
}

impl Default for PiiRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiRedactor {
    /// Create a PII redactor from a list of pattern configs (from DynamicConfig).
    pub fn from_config(configs: &[PiiPatternConfig]) -> Self {
        let patterns = configs
            .iter()
            .filter_map(|c| match Regex::new(&c.regex) {
                Ok(regex) => Some(PiiPattern {
                    name: c.name.clone(),
                    regex,
                    placeholder_prefix: c.placeholder_prefix.clone(),
                }),
                Err(e) => {
                    tracing::warn!("Invalid PII regex pattern '{}': {e}", c.name);
                    None
                }
            })
            .collect();
        Self { patterns }
    }

    pub fn new() -> Self {
        // Static compiled regexes — compiled once, reused across all
        // PiiRedactor instances and requests.
        static RE_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap()
        });
        static RE_ID_CARD_CN: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b\d{17}[\dXx]\b").unwrap());
        static RE_CREDIT_CARD: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b").unwrap());
        static RE_PHONE_CN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"1[3-9]\d{9}").unwrap());
        static RE_PHONE_US: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b\d{3}[-.]?\d{3}[-.]?\d{4}\b").unwrap());
        static RE_IPV4: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap());

        // Order matters: longer/more specific patterns must come before shorter ones
        // to prevent partial matches (e.g. phone patterns matching inside credit cards).
        let patterns = vec![
            PiiPattern {
                name: "email".into(),
                regex: RE_EMAIL.clone(),
                placeholder_prefix: "EMAIL".into(),
            },
            PiiPattern {
                name: "id_card_cn".into(),
                regex: RE_ID_CARD_CN.clone(),
                placeholder_prefix: "ID".into(),
            },
            PiiPattern {
                name: "credit_card".into(),
                regex: RE_CREDIT_CARD.clone(),
                placeholder_prefix: "CARD".into(),
            },
            PiiPattern {
                name: "phone_cn".into(),
                regex: RE_PHONE_CN.clone(),
                placeholder_prefix: "PHONE".into(),
            },
            PiiPattern {
                name: "phone_us".into(),
                regex: RE_PHONE_US.clone(),
                placeholder_prefix: "PHONE".into(),
            },
            PiiPattern {
                name: "ipv4".into(),
                regex: RE_IPV4.clone(),
                placeholder_prefix: "IP".into(),
            },
        ];

        Self { patterns }
    }

    /// Redact PII from user messages, returning modified messages and a context
    /// that can be used to restore original values in the response.
    ///
    /// Only `user` role messages are redacted; `system` and `assistant` messages
    /// are left unchanged.
    ///
    /// Uses a single-pass approach: build a combined regex from all patterns,
    /// find all matches with positions, sort by position (descending), and
    /// replace in reverse order to avoid invalidating offsets.
    pub fn redact_messages(
        &self,
        messages: &[ChatMessage],
    ) -> (Vec<ChatMessage>, RedactionContext) {
        let mut counters: HashMap<String, u32> = HashMap::new();
        let mut replacements: HashMap<String, String> = HashMap::new();

        // Per-request random salt to prevent placeholder prediction.
        // 64 bits gives 2^64 possible values — wide enough that an
        // attacker can't enumerate placeholder space across requests
        // to correlate redacted PII.
        let salt: u64 = rand::rng().random();
        let salt_hex = format!("{salt:016x}");

        let redacted = messages
            .iter()
            .map(|msg| {
                if msg.role != "user" {
                    return msg.clone();
                }

                let content_str = match msg.content.as_str() {
                    Some(s) => s.to_string(),
                    None => return msg.clone(),
                };

                // Collect all matches across all patterns with their positions
                let mut all_matches: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, pattern_idx)
                for (pattern_idx, pattern) in self.patterns.iter().enumerate() {
                    for m in pattern.regex.find_iter(&content_str) {
                        all_matches.push((m.start(), m.end(), pattern_idx));
                    }
                }

                if all_matches.is_empty() {
                    return msg.clone();
                }

                // Sort by start position ascending, then by length descending (prefer longer matches)
                all_matches
                    .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));

                // Remove overlapping matches — keep the longest match at each position
                let mut filtered: Vec<(usize, usize, usize)> = Vec::new();
                for m in &all_matches {
                    // Only add if it doesn't overlap with any already-accepted match
                    if filtered.iter().all(|f| m.0 >= f.1 || m.1 <= f.0) {
                        filtered.push(*m);
                    }
                }
                // Sort descending by start for safe replacement
                filtered.sort_by_key(|b| std::cmp::Reverse(b.0));

                // Collect pattern names for logging before consuming filtered
                let redacted_pattern_names: Vec<String> = filtered
                    .iter()
                    .map(|(_, _, idx)| self.patterns[*idx].name.clone())
                    .collect();

                let mut redacted_content = content_str;
                for (start, end, pattern_idx) in filtered {
                    let pattern = &self.patterns[pattern_idx];
                    let matched_value = &redacted_content[start..end];
                    let counter = counters
                        .entry(pattern.placeholder_prefix.clone())
                        .or_insert(0);
                    *counter += 1;
                    let placeholder = format!(
                        "{{{{{}_{}_{}}}}}",
                        pattern.placeholder_prefix, salt_hex, counter
                    );
                    replacements.insert(placeholder.clone(), matched_value.to_string());
                    redacted_content.replace_range(start..end, &placeholder);
                }

                if !redacted_pattern_names.is_empty() {
                    tracing::debug!(
                        patterns = ?redacted_pattern_names,
                        count = redacted_pattern_names.len(),
                        "PII redacted from user message"
                    );
                }

                ChatMessage {
                    role: msg.role.clone(),
                    content: serde_json::Value::String(redacted_content),
                }
            })
            .collect();

        (redacted, RedactionContext { replacements })
    }

    /// Restore placeholders in the response content back to original PII values.
    pub fn restore_response(&self, response: &mut ChatCompletionResponse, ctx: &RedactionContext) {
        if ctx.replacements.is_empty() {
            return;
        }

        for choice in &mut response.choices {
            if let Some(content_str) = choice.message.content.as_str() {
                let mut restored = content_str.to_string();
                for (placeholder, original) in &ctx.replacements {
                    restored = restored.replace(placeholder, original);
                }
                choice.message.content = serde_json::Value::String(restored);
            }
        }
    }
}

/// Stateful restorer for streaming responses. Placeholders have the
/// shape `{{TYPE_SALT_N}}` which a token stream may fragment across
/// arbitrary chunks — `{{` in one chunk and `EMAIL_abc_1}}` in the next.
///
/// The restorer buffers the tail of unflushed content whenever it sees
/// an unclosed `{{` (or a lone trailing `{` that might be the start of
/// one) and releases it as soon as the closing `}}` arrives. All
/// complete placeholders are replaced with their original values before
/// emission; anything that *looks* like a placeholder but doesn't match
/// any known key passes through verbatim.
///
/// Emit ordering is preserved: the concatenation of `process()` outputs
/// plus the final `flush()` equals what `restore_response` would return
/// for the same content seen as a single string.
pub struct PiiStreamRestorer {
    /// Placeholder → original lookup. Cloned out of a RedactionContext
    /// because we need ownership once and it's cheap (typically < 10 entries).
    replacements: HashMap<String, String>,
    /// Unflushed tail that might still grow into a complete placeholder.
    buffer: String,
}

impl PiiStreamRestorer {
    pub fn new(ctx: &RedactionContext) -> Self {
        Self {
            replacements: ctx.replacements.clone(),
            buffer: String::new(),
        }
    }

    /// Returns true when the restorer has no work to do — callers can
    /// short-circuit and pass the chunk through untouched.
    pub fn is_noop(&self) -> bool {
        self.replacements.is_empty()
    }

    /// Feed the next piece of decoded content. Returns whatever is safe
    /// to emit now (placeholders already restored). The unreleased tail
    /// stays in the buffer for the next call.
    pub fn process(&mut self, next: &str) -> String {
        if self.is_noop() {
            // Nothing to restore; never buffer — avoid introducing
            // latency when the feature isn't even active.
            return next.to_string();
        }
        self.buffer.push_str(next);
        let cut = Self::safe_emit_boundary(&self.buffer);
        if cut == 0 {
            return String::new();
        }
        // Emit [0..cut) with replacements; keep [cut..) in the buffer.
        let emit_slice = self.buffer[..cut].to_string();
        let restored = self.restore_complete(&emit_slice);
        self.buffer.drain(..cut);
        restored
    }

    /// Final drain — called once when the source stream ends. Any
    /// residual buffer is emitted verbatim (an unterminated `{{...` at
    /// the very end of a stream never becomes a placeholder, so the
    /// safest thing is to let the client see what the upstream actually
    /// said).
    pub fn flush(&mut self) -> String {
        if self.buffer.is_empty() {
            return String::new();
        }
        let out = self.restore_complete(&self.buffer);
        self.buffer.clear();
        out
    }

    /// Replace every known placeholder in `s` with its original value.
    /// Linear in `s.len() × replacements.len()`; the replacements map
    /// is expected to be small (single-digit entries) so the nested
    /// loop is fine in practice.
    fn restore_complete(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (placeholder, original) in &self.replacements {
            if out.contains(placeholder) {
                out = out.replace(placeholder, original);
            }
        }
        out
    }

    /// Given a buffer, return the byte index up to which it is safe to
    /// emit now. Everything from the returned index onwards must stay
    /// buffered because it might still grow into a `{{...}}` placeholder.
    ///
    /// Rules:
    ///  1. Find the rightmost `{{`. If there is no matching `}}` after
    ///     it, cut there — that `{{` is still open.
    ///  2. Otherwise, if the buffer ends with a single `{`, cut one
    ///     byte back so the next chunk's leading `{` can join it.
    ///  3. Otherwise, the whole buffer is releasable.
    fn safe_emit_boundary(buf: &str) -> usize {
        let bytes = buf.as_bytes();
        if let Some(open_pos) = buf.rfind("{{") {
            // Is there a `}}` strictly after the `{{`? Start looking
            // two bytes past the `{{` so a literal `{{}}` doesn't
            // match itself (nonsense but cheap to guard).
            let after_open = open_pos + 2;
            if after_open >= bytes.len() {
                // `{{` at the very end → definitely still open.
                return open_pos;
            }
            if buf[after_open..].contains("}}") {
                // Complete placeholder — fall through to the trailing-
                // `{` check so we don't release a lone brace.
            } else {
                return open_pos;
            }
        }
        // No unclosed `{{`. But a single trailing `{` could be the
        // first half of a future `{{` — hold it back by one byte.
        if bytes.last() == Some(&b'{') {
            return bytes.len() - 1;
        }
        bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatCompletionResponse, ChatMessage, Choice, Usage};

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(content.to_string()),
        }
    }

    fn system_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(content.to_string()),
        }
    }

    fn make_response(content: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(content.to_string()),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
            }),
        }
    }

    /// Find the placeholder replacement that maps to the given original value.
    fn find_placeholder(ctx: &RedactionContext, original: &str) -> String {
        ctx.replacements
            .iter()
            .find(|(_, v)| v.as_str() == original)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| panic!("no placeholder for {original}"))
    }

    #[test]
    fn redact_email() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Contact me at alice@example.com please")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_phone() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Call me at 13812345678")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("PHONE"), "got: {content}");
        assert!(!content.contains("13812345678"));
        let ph = find_placeholder(&ctx, "13812345678");
        assert!(ph.starts_with("{{PHONE_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_us_phone() {
        let redactor = PiiRedactor::new();
        // Simplified US phone regex matches 10-digit patterns like 555-123-4567
        let messages = vec![user_msg("Call 555-123-4567")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(
            content.contains("PHONE"),
            "phone should be redacted, got: {content}"
        );
        assert!(!content.contains("123-4567"));
    }

    #[test]
    fn redact_credit_card() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("My card is 4111-1111-1111-1111")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("4111"));
        let ph = find_placeholder(&ctx, "4111-1111-1111-1111");
        assert!(ph.starts_with("{{CARD_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_id_card() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("ID: 110101199001011234")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("ID"), "got: {content}");
        assert!(!content.contains("110101199001011234"));
        let ph = find_placeholder(&ctx, "110101199001011234");
        assert!(ph.starts_with("{{ID_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_ipv4() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Server is at 192.168.1.100")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("IP"), "got: {content}");
        assert!(!content.contains("192.168.1.100"));
        let ph = find_placeholder(&ctx, "192.168.1.100");
        assert!(ph.starts_with("{{IP_"), "placeholder format: {ph}");
    }

    #[test]
    fn does_not_redact_system_messages() {
        let redactor = PiiRedactor::new();
        let messages = vec![system_msg("Contact admin@example.com for help")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("admin@example.com"));
    }

    #[test]
    fn restore_response_replaces_placeholders() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Email alice@example.com and bob@test.org")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        // Simulate the LLM echoing back the redacted content
        let redacted_content = redacted[0].content.as_str().unwrap();
        let mut response = make_response(redacted_content);
        redactor.restore_response(&mut response, &ctx);

        let content = response.choices[0].message.content.as_str().unwrap();
        assert!(content.contains("alice@example.com"), "got: {content}");
        assert!(content.contains("bob@test.org"), "got: {content}");
        assert!(!content.contains("{{EMAIL_"));
    }

    #[test]
    fn salt_is_64_bit_hex() {
        // The wave-4 salt widening from u16 to u64 changes the
        // placeholder format from `{{EMAIL_xxxx_1}}` (4 hex chars)
        // to `{{EMAIL_xxxxxxxxxxxxxxxx_1}}` (16 hex chars). A
        // regression to the narrow salt would re-open the
        // collision-correlation gap the wave-4 review flagged.
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Reach me at alice@example.com")];
        let (_redacted, ctx) = redactor.redact_messages(&messages);
        let placeholder = find_placeholder(&ctx, "alice@example.com");
        // Format: `{{EMAIL_<16 hex>_<counter>}}`
        let inside = placeholder
            .strip_prefix("{{EMAIL_")
            .and_then(|s| s.strip_suffix("}}"))
            .expect("placeholder format unexpected");
        let parts: Vec<&str> = inside.split('_').collect();
        assert_eq!(parts.len(), 2, "expected SALT_COUNTER, got {placeholder}");
        assert_eq!(
            parts[0].len(),
            16,
            "salt must be 16 hex chars (64 bits), got {} chars in {placeholder}",
            parts[0].len()
        );
        assert!(
            parts[0].chars().all(|c| c.is_ascii_hexdigit()),
            "salt must be hex: {placeholder}"
        );
    }

    #[test]
    fn salt_differs_per_request() {
        // Each redact_messages call generates a fresh salt, so the
        // same email in two different requests gets two different
        // placeholders. The narrow u16 salt had a 65k collision
        // space; the new u64 should never collide in practice.
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("alice@example.com")];
        let (_, ctx_a) = redactor.redact_messages(&messages);
        let (_, ctx_b) = redactor.redact_messages(&messages);
        let ph_a = find_placeholder(&ctx_a, "alice@example.com");
        let ph_b = find_placeholder(&ctx_b, "alice@example.com");
        assert_ne!(
            ph_a, ph_b,
            "salt must differ across requests; got identical {ph_a}"
        );
    }

    #[test]
    fn multiple_pii_types() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg(
            "Email alice@example.com, IP 10.0.0.1, card 4111 1111 1111 1111",
        )];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(content.contains("IP"), "got: {content}");
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        assert!(!content.contains("10.0.0.1"));

        // Verify restore round-trip
        let mut response = make_response(content);
        redactor.restore_response(&mut response, &ctx);
        let restored = response.choices[0].message.content.as_str().unwrap();
        assert!(restored.contains("alice@example.com"), "got: {restored}");
        assert!(restored.contains("10.0.0.1"), "got: {restored}");
    }

    #[test]
    fn from_config_loads_patterns() {
        let configs = vec![PiiPatternConfig {
            name: "email_custom".into(),
            regex: r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}".into(),
            placeholder_prefix: "CUSTOM_EMAIL".into(),
        }];
        let redactor = PiiRedactor::from_config(&configs);

        let messages = vec![user_msg("Contact test@example.com for info")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("CUSTOM_EMAIL"), "got: {content}");
        assert!(!content.contains("test@example.com"));
        let ph = find_placeholder(&ctx, "test@example.com");
        assert!(
            ph.starts_with("{{CUSTOM_EMAIL_"),
            "placeholder format: {ph}"
        );
    }

    #[test]
    fn from_config_invalid_regex_skipped() {
        let configs = vec![
            PiiPatternConfig {
                name: "bad_regex".into(),
                regex: r"[invalid((".into(), // malformed regex
                placeholder_prefix: "BAD".into(),
            },
            PiiPatternConfig {
                name: "good_email".into(),
                regex: r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}".into(),
                placeholder_prefix: "EMAIL".into(),
            },
        ];
        // Should not panic — invalid regex is skipped
        let redactor = PiiRedactor::from_config(&configs);

        // The valid pattern should still work
        let messages = vec![user_msg("Contact me at alice@test.org")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);
        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@test.org"));
    }

    // ---------------------------------------------------------------
    // PiiStreamRestorer — rebuilds restored text across arbitrary chunk
    // boundaries. The invariant we're testing:
    //   concat(restorer.process(chunk_i) for i in 0..N) + restorer.flush()
    //   == restore_complete(concat(chunk_i))
    // ---------------------------------------------------------------

    fn sample_ctx() -> RedactionContext {
        let mut r = HashMap::new();
        r.insert("{{EMAIL_abc123_1}}".into(), "alice@example.com".into());
        r.insert("{{PHONE_def456_1}}".into(), "13812345678".into());
        RedactionContext { replacements: r }
    }

    fn restore_whole(chunks: &[&str]) -> String {
        let ctx = sample_ctx();
        let mut r = PiiStreamRestorer::new(&ctx);
        let mut out = String::new();
        for c in chunks {
            out.push_str(&r.process(c));
        }
        out.push_str(&r.flush());
        out
    }

    #[test]
    fn stream_restore_handles_whole_placeholder_in_one_chunk() {
        let out = restore_whole(&["Hi {{EMAIL_abc123_1}}!"]);
        assert_eq!(out, "Hi alice@example.com!");
    }

    #[test]
    fn stream_restore_reassembles_placeholder_split_across_chunks() {
        // Split right after the opening `{{`.
        let out = restore_whole(&["Hi {{", "EMAIL_abc123_1}}!"]);
        assert_eq!(out, "Hi alice@example.com!");
    }

    #[test]
    fn stream_restore_reassembles_single_byte_split() {
        // Every boundary case at once — one byte per chunk.
        let input = "{{EMAIL_abc123_1}}";
        let chunks: Vec<String> = input.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
        let out = restore_whole(&refs);
        assert_eq!(out, "alice@example.com");
    }

    #[test]
    fn stream_restore_handles_trailing_lone_brace() {
        // The first chunk ends with a single `{` — it might be the
        // start of a placeholder. Must hold it back.
        let out = restore_whole(&["prefix {", "{EMAIL_abc123_1}} tail"]);
        assert_eq!(out, "prefix alice@example.com tail");
    }

    #[test]
    fn stream_restore_passes_unknown_placeholder_like_tokens_through() {
        // The model echoed something that *looks* like a placeholder
        // but isn't in the replacements map. Must flow through as-is
        // after the closing `}}`, not stay buffered forever.
        let out = restore_whole(&["see {{NOT_", "A_REAL_KEY}} done"]);
        assert_eq!(out, "see {{NOT_A_REAL_KEY}} done");
    }

    #[test]
    fn stream_restore_flush_emits_unterminated_tail_verbatim() {
        // Upstream ended mid-placeholder. We don't silently drop the
        // tail — emit it so the client at least sees something.
        let out = restore_whole(&["oops {{EMAIL_incompl"]);
        assert_eq!(out, "oops {{EMAIL_incompl");
    }

    #[test]
    fn stream_restore_noop_when_context_is_empty() {
        let ctx = RedactionContext {
            replacements: HashMap::new(),
        };
        let mut r = PiiStreamRestorer::new(&ctx);
        assert!(r.is_noop());
        // Even with a `{{` in the input, no buffering happens — we
        // want zero latency overhead when the feature isn't active.
        let out1 = r.process("partial {{foo");
        assert_eq!(out1, "partial {{foo");
        let out2 = r.process(" bar}}");
        assert_eq!(out2, " bar}}");
        assert_eq!(r.flush(), "");
    }

    #[test]
    fn stream_restore_anthropic_style_fragmented_deltas() {
        // Mimics Anthropic `content_block_delta` events that each carry
        // one or two tokens. Placeholders can land on any boundary.
        let out = restore_whole(&[
            "Hello ",
            "{{",
            "EMAIL_",
            "abc123_1",
            "}}",
            " and ",
            "{{PHONE_def456_1}}",
            ".",
        ]);
        assert_eq!(out, "Hello alice@example.com and 13812345678.");
    }

    #[test]
    fn stream_restore_multiple_placeholders_same_chunk() {
        let out = restore_whole(&["a {{EMAIL_abc123_1}} b {{PHONE_def456_1}} c"]);
        assert_eq!(out, "a alice@example.com b 13812345678 c");
    }
}
