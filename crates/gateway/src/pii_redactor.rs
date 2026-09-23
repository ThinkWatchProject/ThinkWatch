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
    /// Maps placeholder (e.g. `{{EMAIL_1}}`) to original value.
    pub replacements: HashMap<String, String>,
}

/// Keys that carry base64 in a request. Replacement never enters them: a
/// digit run landing inside an encoded image is unlikely, but where it
/// happens the thing changed is the image, not the PII.
const BASE64_CARRIERS: &[&str] = &["data", "bytes"];

impl RedactionContext {
    /// Carry the found PII onto a **raw** request.
    ///
    /// A request forwarded in its own format never goes through the
    /// decoded form — that is how `cache_control` and everything else
    /// the decoded form does not model survive. But PII is found on the
    /// decoded form, where the structure is known, so the
    /// value → placeholder mapping has to be carried back onto the raw
    /// JSON.
    ///
    /// **On the parsed `Value`, not the bytes**: a client may send `@` as
    /// `\u0040`, and the bytes would not contain the value at all.
    ///
    /// Longer values first, so `a@x.com` does not eat part of `aa@x.com`.
    ///
    /// A value that also appears in the system prompt is replaced there
    /// too — which only happens when the caller also wrote it.
    pub fn apply_to(&self, value: &mut serde_json::Value) {
        if self.replacements.is_empty() {
            return;
        }
        let mut pairs: Vec<(&str, &str)> = self
            .replacements
            .iter()
            .map(|(ph, orig)| (orig.as_str(), ph.as_str()))
            .collect();
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)));
        walk_strings(value, &mut |s| {
            for (orig, ph) in &pairs {
                if s.contains(orig) {
                    *s = s.replace(orig, ph);
                }
            }
        });
    }

    /// Paint the original values back into a whole response's bytes.
    ///
    /// A whole response has its placeholders intact, so this works on the
    /// bytes. Each original is JSON-escaped first — one containing a quote,
    /// put back as-is, would break the document. A stream cannot be done
    /// this way: a placeholder split across two frames is not contiguous in
    /// the byte stream (see [`PiiStreamRestorer`]).
    pub fn restore_bytes(&self, body: &[u8]) -> Vec<u8> {
        if self.replacements.is_empty() {
            return body.to_vec();
        }
        let mut text = String::from_utf8_lossy(body).into_owned();
        for (ph, orig) in &self.replacements {
            if text.contains(ph.as_str()) {
                let escaped = serde_json::to_string(orig).unwrap_or_default();
                // Drop the quotes `to_string` added; keep the escaping.
                let inner = &escaped[1..escaped.len().saturating_sub(1)];
                text = text.replace(ph.as_str(), inner);
            }
        }
        text.into_bytes()
    }
}

fn walk_strings(v: &mut serde_json::Value, f: &mut impl FnMut(&mut String)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| walk_strings(i, f)),
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if !BASE64_CARRIERS.contains(&k.as_str()) {
                    walk_strings(child, f);
                }
            }
        }
        _ => {}
    }
}

impl Default for PiiRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiRedactor {
    /// Create a PII redactor from a list of pattern configs (from DynamicConfig).
    ///
    /// Each pattern is compiled through
    /// `think_watch_common::regex_util::compile_bounded` so an operator
    /// who saves a pathological pattern can't DOS the redactor —
    /// every gateway request would otherwise pay seconds of regex
    /// engine work per inbound message.
    pub fn from_config(configs: &[PiiPatternConfig]) -> Self {
        let patterns = configs
            .iter()
            .filter_map(
                |c| match think_watch_common::regex_util::compile_bounded(&c.regex) {
                    Ok(regex) => Some(PiiPattern {
                        name: c.name.clone(),
                        regex,
                        placeholder_prefix: c.placeholder_prefix.clone(),
                    }),
                    Err(e) => {
                        // Save-time validation in admin/settings should prevent
                        // invalid patterns from ever reaching us. If one shows
                        // up here it means the DB row was hand-edited or the
                        // validator drifted — either way, surface loudly so
                        // operators don't think PII redaction is on when it
                        // silently isn't.
                        tracing::error!(
                            pattern = %c.name,
                            error = %e,
                            "Invalid PII regex — pattern is DISABLED for redaction"
                        );
                        metrics::counter!(
                            "gateway_pii_pattern_invalid_total",
                            "pattern" => c.name.clone(),
                        )
                        .increment(1);
                        None
                    }
                },
            )
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

    /// Redact one piece of text. For the admin "try these patterns"
    /// endpoint, and anything else that holds plain text rather than a
    /// request.
    pub fn redact_str(&self, text: &str) -> (String, RedactionContext) {
        let mut counters = HashMap::new();
        let mut replacements = HashMap::new();
        let out = self.redact_text(text, &mut counters, &mut replacements, "text");
        (out, RedactionContext { replacements })
    }

    /// Redact the caller's text in a decoded request.
    ///
    /// The decoded form's structure is known, which the earlier version —
    /// guessing at a `serde_json::Value` for a string or a `text` field —
    /// never had: it missed text nested in Anthropic `tool_result` blocks,
    /// the array form of `system`, and Responses parts whose text field is
    /// not called `text`.
    ///
    /// Only user messages are redacted; assistant turns pass through.
    ///
    /// `Request.system` is not redacted. The system prompt is written by
    /// the operator, not typed by the caller; redacting an address or IP
    /// in it rewrites the operator's instructions, and such values there
    /// are configuration, not user PII.
    pub fn redact_request(&self, request: &mut tw_dialect::ir::Request) -> RedactionContext {
        use tw_dialect::ir::Role;

        let mut counters: HashMap<String, u32> = HashMap::new();
        let mut replacements: HashMap<String, String> = HashMap::new();

        for msg in &mut request.messages {
            if msg.role != Role::User {
                continue;
            }
            self.redact_parts(
                &mut msg.parts,
                &mut counters,
                &mut replacements,
                "user message",
            );
        }

        RedactionContext { replacements }
    }

    /// Apply the redaction patterns to a single text blob. Shared
    /// between the single-string and multimodal-array branches of
    /// `redact_messages` so both shapes get identical treatment.
    fn redact_text(
        &self,
        content_str: &str,
        counters: &mut HashMap<String, u32>,
        replacements: &mut HashMap<String, String>,
        log_origin: &str,
    ) -> String {
        let mut all_matches: Vec<(usize, usize, usize)> = Vec::new();
        for (pattern_idx, pattern) in self.patterns.iter().enumerate() {
            for m in pattern.regex.find_iter(content_str) {
                all_matches.push((m.start(), m.end(), pattern_idx));
            }
        }

        if all_matches.is_empty() {
            return content_str.to_string();
        }

        all_matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));

        let mut filtered: Vec<(usize, usize, usize)> = Vec::new();
        for m in &all_matches {
            if filtered.iter().all(|f| m.0 >= f.1 || m.1 <= f.0) {
                filtered.push(*m);
            }
        }
        filtered.sort_by_key(|b| std::cmp::Reverse(b.0));

        let redacted_pattern_names: Vec<String> = filtered
            .iter()
            .map(|(_, _, idx)| self.patterns[*idx].name.clone())
            .collect();

        let mut redacted_content = content_str.to_string();
        for (start, end, pattern_idx) in filtered {
            let pattern = &self.patterns[pattern_idx];
            let matched_value = redacted_content[start..end].to_string();
            // One placeholder per value. A model shown `{{EMAIL_1}}` and
            // `{{EMAIL_2}}` treats them as two people; and a forwarded
            // request needs value → placeholder to be a function to carry
            // it onto the raw JSON.
            let prefix = format!("{{{{{}_", pattern.placeholder_prefix);
            let existing = replacements
                .iter()
                .find(|(ph, orig)| ph.starts_with(&prefix) && **orig == matched_value)
                .map(|(ph, _)| ph.clone());
            let placeholder = match existing {
                Some(ph) => ph,
                None => {
                    let counter = counters
                        .entry(pattern.placeholder_prefix.clone())
                        .or_insert(0);
                    *counter += 1;
                    let ph = format!("{{{{{}_{}}}}}", pattern.placeholder_prefix, counter);
                    replacements.insert(ph.clone(), matched_value);
                    ph
                }
            };
            redacted_content.replace_range(start..end, &placeholder);
        }

        if !redacted_pattern_names.is_empty() {
            tracing::debug!(
                patterns = ?redacted_pattern_names,
                count = redacted_pattern_names.len(),
                origin = log_origin,
                "PII redacted"
            );
        }

        redacted_content
    }

    /// The recursive part of [`Self::redact_request`]: redact a list of
    /// parts in place.
    ///
    /// `Part::ToolResult` is recursed into. Tool results often carry data
    /// a tool fetched on the user's behalf — a mailbox, an order — and the
    /// earlier `Value`-based redactor had no notion of a tool result at
    /// all.
    ///
    /// `Image` / `File` / `Thinking` / `ToolCall` are left alone: media is
    /// not redactable text, thinking is the model's own reasoning, and
    /// changing `ToolCall.input` would break the call itself.
    fn redact_parts(
        &self,
        parts: &mut [tw_dialect::ir::Part],
        counters: &mut HashMap<String, u32>,
        replacements: &mut HashMap<String, String>,
        log_origin: &str,
    ) {
        use tw_dialect::ir::Part;

        for part in parts {
            match part {
                Part::Text(s) => {
                    *s = self.redact_text(s, counters, replacements, log_origin);
                }
                Part::ToolResult(r) => {
                    self.redact_parts(
                        &mut r.content,
                        counters,
                        replacements,
                        "user message (tool result)",
                    );
                }
                Part::Image(_) | Part::File { .. } | Part::Thinking(_) | Part::ToolCall(_) => {}
            }
        }
    }

    /// Apply redaction patterns to an arbitrary serialized blob (e.g.
    /// a JSON string going into the audit log). Drops the per-match
    /// restoration context — the result is write-only audit data,
    /// never round-tripped back to a caller, so we replace with the
    /// pattern name alone instead of a position-salted placeholder.
    ///
    /// Used by the body-capture pipeline when an operator sets
    /// `audit.body_redact_pii = true`. Distinct from
    /// `redact_messages` which is the in-flight redactor that DOES
    /// need a restoration context so the user's own response can be
    /// painted with the original PII.
    pub fn redact_blob(&self, input: &str) -> String {
        if self.patterns.is_empty() {
            return input.to_string();
        }
        // Gather all matches first so overlapping patterns get a
        // deterministic non-overlapping resolution (longest match
        // wins on tie) — same algorithm as `redact_text` to keep the
        // in-flight and at-rest redaction story consistent.
        let mut all_matches: Vec<(usize, usize, usize)> = Vec::new();
        for (pattern_idx, pattern) in self.patterns.iter().enumerate() {
            for m in pattern.regex.find_iter(input) {
                all_matches.push((m.start(), m.end(), pattern_idx));
            }
        }
        if all_matches.is_empty() {
            return input.to_string();
        }
        all_matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));
        let mut filtered: Vec<(usize, usize, usize)> = Vec::new();
        for m in &all_matches {
            if filtered.iter().all(|f| m.0 >= f.1 || m.1 <= f.0) {
                filtered.push(*m);
            }
        }
        filtered.sort_by_key(|b| std::cmp::Reverse(b.0));

        let mut result = input.to_string();
        for (start, end, pattern_idx) in filtered {
            let replacement = format!("{{{{REDACTED_{}}}}}", self.patterns[pattern_idx].name);
            result.replace_range(start..end, &replacement);
        }
        result
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

    /// One-shot restoration for a string that is NOT part of the
    /// streaming content path (typically an error message or a cached
    /// chunk). Does not touch the internal buffer, so a successful
    /// chunk's unflushed tail survives — important when an upstream
    /// error interrupts a stream mid-placeholder and we still want the
    /// trailing `flush()` to behave correctly.
    pub fn restore_oneshot(&self, s: &str) -> String {
        self.restore_complete(s)
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

    /// Find the placeholder replacement that maps to the given original value.
    #[test]
    fn applying_to_a_raw_request_touches_nothing_but_the_redacted_text() {
        // A request forwarded as sent must reach the upstream whole —
        // `name`, `cache_control`, everything — apart from the PII.
        let ctx = RedactionContext {
            replacements: [("{{EMAIL_1}}".to_string(), "alice@example.com".to_string())]
                .into_iter()
                .collect(),
        };
        let mut v = serde_json::json!({
            "role": "user", "name": "alice",
            "content": [{"type": "text", "text": "mail alice@example.com",
                         "cache_control": {"type": "ephemeral"}}]
        });
        ctx.apply_to(&mut v);
        assert_eq!(v["name"], "alice");
        assert_eq!(v["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(v["content"][0]["text"], "mail {{EMAIL_1}}");
    }

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
        let (redacted, ctx) = redactor.redact_str("Contact me at alice@example.com please");

        let content = redacted.as_str();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_phone() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) = redactor.redact_str("Call me at 13812345678");

        let content = redacted.as_str();
        assert!(content.contains("PHONE"), "got: {content}");
        assert!(!content.contains("13812345678"));
        let ph = find_placeholder(&ctx, "13812345678");
        assert!(ph.starts_with("{{PHONE_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_us_phone() {
        let redactor = PiiRedactor::new();
        // Simplified US phone regex matches 10-digit patterns like 555-123-4567
        let (redacted, _ctx) = redactor.redact_str("Call 555-123-4567");

        let content = redacted.as_str();
        assert!(
            content.contains("PHONE"),
            "phone should be redacted, got: {content}"
        );
        assert!(!content.contains("123-4567"));
    }

    #[test]
    fn redact_credit_card() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) = redactor.redact_str("My card is 4111-1111-1111-1111");

        let content = redacted.as_str();
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("4111"));
        let ph = find_placeholder(&ctx, "4111-1111-1111-1111");
        assert!(ph.starts_with("{{CARD_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_id_card() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) = redactor.redact_str("ID: 110101199001011234");

        let content = redacted.as_str();
        assert!(content.contains("ID"), "got: {content}");
        assert!(!content.contains("110101199001011234"));
        let ph = find_placeholder(&ctx, "110101199001011234");
        assert!(ph.starts_with("{{ID_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_ipv4() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) = redactor.redact_str("Server is at 192.168.1.100");

        let content = redacted.as_str();
        assert!(content.contains("IP"), "got: {content}");
        assert!(!content.contains("192.168.1.100"));
        let ph = find_placeholder(&ctx, "192.168.1.100");
        assert!(ph.starts_with("{{IP_"), "placeholder format: {ph}");
    }

    #[test]
    fn restore_response_replaces_placeholders() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) = redactor.redact_str("Email alice@example.com and bob@test.org");

        // Simulate the LLM echoing back the redacted content
        let redacted_content = redacted.as_str();
        let content = String::from_utf8(ctx.restore_bytes(redacted_content.as_bytes())).unwrap();
        assert!(content.contains("alice@example.com"), "got: {content}");
        assert!(content.contains("bob@test.org"), "got: {content}");
        assert!(!content.contains("{{EMAIL_"));
    }

    #[test]
    fn placeholders_are_stable_counter_only() {
        // Stable placeholder format: `{{EMAIL_<counter>}}`. The salt
        // was dropped intentionally — see DESIGN-001 in proxy.rs —
        // so that two callers with identical pre-redaction prompts
        // produce identical redacted bodies, allowing the response
        // cache to actually hit. Two callers with identical text
        // must also have identical contexts (PII values come from
        // the text itself), so the symmetry is safe.
        let redactor = PiiRedactor::new();
        let (_redacted, ctx) = redactor.redact_str("Reach me at alice@example.com");
        let placeholder = find_placeholder(&ctx, "alice@example.com");
        assert_eq!(
            placeholder, "{{EMAIL_1}}",
            "placeholder must be stable counter-only form"
        );
    }

    #[test]
    fn placeholders_are_identical_across_two_calls_with_same_input() {
        // The cache keys on the redacted request and stores the
        // placeholder-form response; two callers sharing a slot only
        // works if redaction is deterministic on the input.
        let redactor = PiiRedactor::new();
        let (_, ctx_a) = redactor.redact_str("alice@example.com");
        let (_, ctx_b) = redactor.redact_str("alice@example.com");
        let ph_a = find_placeholder(&ctx_a, "alice@example.com");
        let ph_b = find_placeholder(&ctx_b, "alice@example.com");
        assert_eq!(
            ph_a, ph_b,
            "redaction must be deterministic so cache hits restore correctly"
        );
    }

    #[test]
    fn multiple_pii_types() {
        let redactor = PiiRedactor::new();
        let (redacted, ctx) =
            redactor.redact_str("Email alice@example.com, IP 10.0.0.1, card 4111 1111 1111 1111");

        let content = redacted.as_str();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(content.contains("IP"), "got: {content}");
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        assert!(!content.contains("10.0.0.1"));

        // Verify restore round-trip
        let restored = String::from_utf8(ctx.restore_bytes(content.as_bytes())).unwrap();
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

        let (redacted, ctx) = redactor.redact_str("Contact test@example.com for info");

        let content = redacted.as_str();
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
        let (redacted, _ctx) = redactor.redact_str("Contact me at alice@test.org");
        let content = redacted.as_str();
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

    // ── redact_request: redaction on the decoded request ──────────────
    use tw_dialect::ir::{Message, Part, Request, Role, ToolResult};

    fn ir_user_message(parts: Vec<Part>) -> Message {
        Message {
            role: Role::User,
            parts,
        }
    }

    fn ir_assistant_message(parts: Vec<Part>) -> Message {
        Message {
            role: Role::Assistant,
            parts,
        }
    }

    fn ir_request(messages: Vec<Message>) -> Request {
        Request {
            model: "test".into(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn redact_request_redacts_a_plain_text_part_in_a_user_message() {
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_user_message(vec![Part::Text(
            "Email me at alice@example.com".into(),
        )])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(text) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        assert!(text.contains("EMAIL"), "got: {text}");
        assert!(!text.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"));
    }

    /// Pins the hole in the earlier version, which guessed at a
    /// `serde_json::Value` and had no notion of a tool result, so text
    /// nested in one went through unredacted.
    /// Tool results are exactly where user data sits — a mailbox, an
    /// order — fed back into the same conversation.
    #[test]
    fn redact_request_redacts_pii_nested_inside_a_tool_result() {
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_user_message(vec![Part::ToolResult(ToolResult {
            id: "call_1".into(),
            content: vec![Part::Text(
                "Found the order, shipped to alice@example.com".into(),
            )],
            is_error: false,
        })])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::ToolResult(result) = &request.messages[0].parts[0] else {
            panic!("expected a tool result part");
        };
        let Part::Text(text) = &result.content[0] else {
            panic!("expected a text part inside the tool result");
        };
        assert!(text.contains("EMAIL"), "got: {text}");
        assert!(!text.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"));
    }

    #[test]
    fn redact_request_does_not_redact_assistant_messages() {
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_assistant_message(vec![Part::Text(
            "Sure, contact alice@example.com".into(),
        )])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(text) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        assert_eq!(text, "Sure, contact alice@example.com");
        assert!(ctx.replacements.is_empty());
    }

    /// The system prompt is the operator's, not the caller's: redacting
    /// it rewrites the instructions, and values there are configuration.
    #[test]
    fn redact_request_does_not_redact_the_system_prompt() {
        let redactor = PiiRedactor::new();
        let mut request = Request {
            model: "test".into(),
            system: vec!["Escalate to ops@example.com when unsure.".into()],
            messages: vec![ir_user_message(vec![Part::Text("hi".into())])],
            ..Default::default()
        };

        redactor.redact_request(&mut request);

        assert_eq!(
            request.system[0],
            "Escalate to ops@example.com when unsure."
        );
    }

    /// A value gets the same placeholder whether it sits in plain text or
    /// inside a tool result — restoration depends on that mapping.
    #[test]
    fn a_value_repeated_across_a_tool_result_restores_everywhere() {
        // The same value twice gets one placeholder, and both places restore
        // — including the one inside the tool result.
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_user_message(vec![
            Part::Text("Contact alice@example.com".into()),
            Part::ToolResult(ToolResult {
                id: "call_1".into(),
                content: vec![Part::Text("Confirmed: alice@example.com".into())],
                is_error: false,
            }),
        ])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(first) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        let Part::ToolResult(result) = &request.messages[0].parts[1] else {
            panic!("expected a tool result part");
        };
        let Part::Text(second) = &result.content[0] else {
            panic!("expected a text part inside the tool result");
        };

        assert!(!first.contains("alice@example.com"), "{first}");
        assert!(!second.contains("alice@example.com"), "{second}");
        assert_eq!(
            first.strip_prefix("Contact "),
            second.strip_prefix("Confirmed: "),
            "the same value should get the same placeholder"
        );

        let restore = |s: &str| {
            ctx.replacements
                .iter()
                .fold(s.to_string(), |acc, (ph, orig)| acc.replace(ph, orig))
        };
        assert_eq!(restore(first), "Contact alice@example.com");
        assert_eq!(restore(second), "Confirmed: alice@example.com");
    }
    #[test]
    fn the_same_value_gets_the_same_placeholder() {
        // Two placeholders read as two people to a model, and a forwarded
        // request needs value → placeholder to be a function.
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_user_message(vec![Part::Text(
            "to a@example.com, cc a@example.com, bcc b@example.com".into(),
        )])]);
        let ctx = redactor.redact_request(&mut request);
        assert_eq!(ctx.replacements.len(), 2, "{:?}", ctx.replacements);
    }

    #[test]
    fn applying_to_a_raw_request_reaches_text_the_client_escaped() {
        // A client may send `\u0040`, and then the bytes hold no `@`.
        // On the parsed Value the string is already unescaped.
        let redactor = PiiRedactor::new();
        let mut ir = ir_request(vec![ir_user_message(vec![Part::Text(
            "mail a@example.com".into(),
        )])]);
        let ctx = redactor.redact_request(&mut ir);

        let raw = r#"{"messages":[{"role":"user","content":"mail a\u0040example.com"}]}"#;
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        ctx.apply_to(&mut v);
        let text = v["messages"][0]["content"].as_str().unwrap();
        assert!(!text.contains("a@example.com"), "{text}");
        assert!(text.starts_with("mail {{EMAIL_"), "{text}");
    }

    #[test]
    fn applying_to_a_raw_request_leaves_base64_alone() {
        let ctx = RedactionContext {
            replacements: [("{{PHONE_1}}".to_string(), "13800138000".to_string())]
                .into_iter()
                .collect(),
        };
        let mut v = serde_json::json!({
            "content": [
                {"type": "text", "text": "call 13800138000"},
                {"type": "image", "source": {"type": "base64", "data": "AB13800138000CD"}}
            ]
        });
        ctx.apply_to(&mut v);
        assert_eq!(v["content"][0]["text"], "call {{PHONE_1}}");
        assert_eq!(
            v["content"][1]["source"]["data"], "AB13800138000CD",
            "that would change the image, not the PII"
        );
    }

    #[test]
    fn the_longer_value_is_replaced_first() {
        let ctx = RedactionContext {
            replacements: [
                ("{{EMAIL_1}}".to_string(), "a@x.com".to_string()),
                ("{{EMAIL_2}}".to_string(), "aa@x.com".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let mut v = serde_json::json!({"text": "aa@x.com and a@x.com"});
        ctx.apply_to(&mut v);
        assert_eq!(v["text"], "{{EMAIL_2}} and {{EMAIL_1}}");
    }

    #[test]
    fn restoring_bytes_escapes_the_original_so_the_json_survives() {
        // An original containing a quote, put back as-is, breaks the JSON.
        let ctx = RedactionContext {
            replacements: [("{{NAME_1}}".to_string(), r#"O"Brien"#.to_string())]
                .into_iter()
                .collect(),
        };
        let body = br#"{"content":[{"type":"text","text":"Hi {{NAME_1}}"}]}"#;
        let out = ctx.restore_bytes(body);
        let v: serde_json::Value = serde_json::from_slice(&out).expect("still valid JSON");
        assert_eq!(v["content"][0]["text"], r#"Hi O"Brien"#);
    }
}
