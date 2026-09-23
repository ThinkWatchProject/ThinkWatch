use crate::providers::traits::{ChatCompletionResponse, ChatMessage};
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

/// 在 JSON 里装 base64 的键。替换不进这些值 —— 一段数字恰好出现在
/// 图片编码里的概率很小，但一旦出现，改掉的是图片，不是 PII。
const BASE64_CARRIERS: &[&str] = &["data", "bytes"];

impl RedactionContext {
    /// 把找到的 PII 替换套到一份**原始**请求上。
    ///
    /// 直通的请求没有经过中间表示 —— 同方言时它原样发出去，才保得住
    /// `cache_control` 这些中间表示不认识的东西。可 PII 是在中间表示上
    /// 找的（那边结构确定），所以要把「值 → 占位符」回套到原始 JSON 上。
    ///
    /// **在解析后的 `Value` 上做，不在字节上做**:客户端可能把字符
    /// 转义成 `\u0040`，字节里就找不到原值了。
    ///
    /// 长的原值先替，免得 `a@x.com` 先把 `aa@x.com` 里的那一截吃掉。
    ///
    /// 同一个值若也出现在系统提示里，那里也会被替换 —— 只在它同时是
    /// 用户写下的 PII 时才会发生。
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

    /// 在一整份响应的字节上把占位符换回原值。
    ///
    /// 整包响应里占位符是完整的，所以可以直接在字节上换。原值要按 JSON
    /// 字符串的规则转义 —— 一个带引号的原值直接塞回去会把 JSON 弄坏。
    /// 流式不能这么做：占位符会被切在两帧里，而两帧之间隔着帧结构，
    /// 在字节流上不连续（见 [`PiiStreamRestorer`]）。
    pub fn restore_bytes(&self, body: &[u8]) -> Vec<u8> {
        if self.replacements.is_empty() {
            return body.to_vec();
        }
        let mut text = String::from_utf8_lossy(body).into_owned();
        for (ph, orig) in &self.replacements {
            if text.contains(ph.as_str()) {
                let escaped = serde_json::to_string(orig).unwrap_or_default();
                // 去掉 to_string 加的那对引号，留下转义好的内容
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

        // Placeholders are stable per request — `{{EMAIL_1}}`,
        // `{{PHONE_2}}`, … — *not* randomised with a per-request
        // salt. Earlier this carried a 64-bit salt to "prevent
        // prediction", but the salt also made cache keys unique
        // per request (cache stores keyed on redacted bytes), so
        // every PII-bearing prompt was a guaranteed cache miss
        // (see DESIGN-001 in proxy.rs). The salt protected against
        // nothing real: cross-caller cache leak requires identical
        // pre-redaction text — but two callers sharing identical
        // pre-redaction text MUST also share identical redaction
        // contexts (the PII values come from the text itself), so
        // restoration is symmetric on either side of the cache.

        let redacted = messages
            .iter()
            .map(|msg| {
                if msg.role != "user" {
                    return msg.clone();
                }

                let new_content = match &msg.content {
                    // OpenAI / Anthropic single-string form
                    serde_json::Value::String(s) => {
                        let redacted =
                            self.redact_text(s, &mut counters, &mut replacements, "user message");
                        serde_json::Value::String(redacted)
                    }
                    // Multimodal form: `[{"type":"text","text":"..."}, {"type":"image_url",...}]`
                    // Each text part is redacted in place; non-text parts (images,
                    // tool_use blocks) pass through unchanged. Without this the
                    // redactor silently bypassed every vision-style request that
                    // contained PII in a text segment.
                    serde_json::Value::Array(parts) => {
                        let new_parts: Vec<serde_json::Value> = parts
                            .iter()
                            .map(|part| match part {
                                serde_json::Value::Object(map) => {
                                    if let Some(serde_json::Value::String(t)) = map.get("text") {
                                        let red = self.redact_text(
                                            t,
                                            &mut counters,
                                            &mut replacements,
                                            "user message (multimodal)",
                                        );
                                        let mut new_map = map.clone();
                                        new_map
                                            .insert("text".into(), serde_json::Value::String(red));
                                        serde_json::Value::Object(new_map)
                                    } else {
                                        part.clone()
                                    }
                                }
                                _ => part.clone(),
                            })
                            .collect();
                        serde_json::Value::Array(new_parts)
                    }
                    other => other.clone(),
                };

                // Preserve `extra` — it carries `name` (OpenAI multi-user
                // chat labels) on user messages, plus any vendor
                // annotations. Replacing only `content` was the bug:
                // `..Default::default()` zeroed the flatten bucket so
                // a name-tagged user prompt got stripped on the way
                // through the redactor.
                ChatMessage {
                    role: msg.role.clone(),
                    content: new_content,
                    extra: msg.extra.clone(),
                }
            })
            .collect();

        (redacted, RedactionContext { replacements })
    }

    /// 在中间表示上脱敏。结构是确定的，不用猜。
    ///
    /// 和 [`Self::redact_messages`] 判的是同一件事，区别只在**文本从哪来**：
    /// 那边要在一个 `serde_json::Value` 上猜哪个字段是文本，猜漏了 Anthropic
    /// 的 `tool_result` 块里嵌套的内容、数组形式的 `system`、Responses 里字段
    /// 名不叫 `text` 的文本部件。这边走 [`tw_dialect::ir`]，结构由类型保证，
    /// 不存在「猜错字段名」这类漏洞。
    ///
    /// 只脱用户侧的内容，和 `redact_messages` 一致：只处理
    /// `Message.role == Role::User`，assistant 消息原样放过。
    ///
    /// `Request.system` 不脱——系统提示是运营方写进配置的，不是调用方输入的；
    /// 脱了系统提示里的邮箱、IP 之类，会把运营方写的指令改样，且这些值本身
    /// 也不是需要保护的用户 PII。
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
            // 同一个值只给一个占位符。两个理由:模型看到 `{{EMAIL_1}}` 和
            // `{{EMAIL_2}}` 会当成两个人;而且直通请求要把「值 → 占位符」
            // 套到原始 JSON 上,那必须是个函数,一个值对两个占位符就无从套
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

    /// [`Self::redact_request`] 的递归部分：就地脱敏一组 IR 部件。
    ///
    /// `Part::ToolResult` 要递归进它自己的 `content`——工具结果里常带着
    /// 模型帮用户查出来的原始数据（读邮件、查订单之类），旧的
    /// `serde_json::Value` 实现没有「工具结果」这个概念，只会漏过去。
    ///
    /// `Image` / `File` / `Thinking` / `ToolCall` 不动：
    /// 图片和文件是二进制媒体，不是可脱敏的文本；`Thinking` 是模型自己的
    /// 推理过程，不是调用方输入；`ToolCall.input` 是模型生成的调用参数，
    /// 改动它会破坏工具调用本身（而且它不是 `redact_messages` 原本处理的
    /// 范围，保持行为一致）。
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
    use crate::providers::traits::{ChatCompletionResponse, ChatMessage, Choice, Usage};

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(content.to_string()),
            ..Default::default()
        }
    }

    fn system_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(content.to_string()),
            ..Default::default()
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
                    ..Default::default()
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
    fn placeholders_are_stable_counter_only() {
        // Stable placeholder format: `{{EMAIL_<counter>}}`. The salt
        // was dropped intentionally — see DESIGN-001 in proxy.rs —
        // so that two callers with identical pre-redaction prompts
        // produce identical redacted bodies, allowing the response
        // cache to actually hit. Two callers with identical text
        // must also have identical contexts (PII values come from
        // the text itself), so the symmetry is safe.
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Reach me at alice@example.com")];
        let (_redacted, ctx) = redactor.redact_messages(&messages);
        let placeholder = find_placeholder(&ctx, "alice@example.com");
        assert_eq!(
            placeholder, "{{EMAIL_1}}",
            "placeholder must be stable counter-only form"
        );
    }

    #[test]
    fn placeholders_are_identical_across_two_calls_with_same_input() {
        // The cache layer keys on pre-redaction content but stores
        // the redacted-form response; for that to work, redaction
        // must be deterministic on the input. This test pins that
        // contract.
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("alice@example.com")];
        let (_, ctx_a) = redactor.redact_messages(&messages);
        let (_, ctx_b) = redactor.redact_messages(&messages);
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

    /// Multimodal user messages (OpenAI vision / Anthropic images)
    /// carry content as an array of typed parts. Without explicit
    /// support, every text segment in such a message bypassed the
    /// redactor — the bug this test pins.
    #[test]
    fn redact_multimodal_text_part() {
        let redactor = PiiRedactor::new();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!([
                { "type": "text", "text": "Email me at alice@example.com" },
                { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
            ]),
            ..Default::default()
        }];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let parts = redacted[0].content.as_array().expect("array preserved");
        assert_eq!(parts.len(), 2);
        let text = parts[0]["text"].as_str().unwrap();
        assert!(text.contains("EMAIL"), "got: {text}");
        assert!(!text.contains("alice@example.com"));
        // Non-text parts pass through unchanged.
        assert_eq!(parts[1]["type"], "image_url");
        // Placeholder is recorded so the response restorer can reverse it.
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"));
    }

    /// `ChatMessage::extra` is a flatten bucket that carries OpenAI
    /// fields the gateway doesn't model explicitly — `name`,
    /// `tool_call_id`, `tool_calls`, vendor annotations. The redactor
    /// rebuilds user messages, and an earlier `..Default::default()`
    /// silently zeroed this bucket, stripping `name` from named user
    /// turns on the way through. Pin the round-trip so the regression
    /// is impossible to reintroduce without breaking this test.
    #[test]
    fn preserves_extra_fields_on_user_messages() {
        let redactor = PiiRedactor::new();
        let mut msg = user_msg("Contact me at alice@example.com");
        msg.extra = serde_json::json!({ "name": "alice" });
        let (redacted, _) = redactor.redact_messages(&[msg]);

        assert_eq!(
            redacted[0].extra.get("name").and_then(|v| v.as_str()),
            Some("alice"),
            "redactor must preserve the OpenAI `name` field on user turns"
        );
        // Content is still redacted — preserving extra didn't disable the body pass.
        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "body got: {content}");
        assert!(!content.contains("alice@example.com"));
    }

    // ── redact_request（IR 上的脱敏） ──────────────────────────────────

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

    /// 钉住旧实现漏掉的洞：`redact_messages` 在 `serde_json::Value` 上猜
    /// 结构，没有「工具结果」这个概念，工具结果里嵌套的内容会原样放过。
    /// 工具结果里恰恰常带用户数据——模型调用一个读邮件、查订单之类的工具，
    /// 结果里原样带着 PII，又被喂回同一次对话。
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

    /// 系统提示是运营方写进配置的，不是调用方输入的：脱了会破坏指令本身，
    /// 而且提示里出现的邮箱、IP 之类通常是有意的配置，不是要保护的用户 PII。
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

    /// 同一个值不管出现在普通文本部件里还是嵌套在工具结果里，都必须拿到
    /// 同一个占位符——还原逻辑靠的就是这份映射，编号一旦分岔就还原不回去。
    #[test]
    fn a_value_repeated_across_a_tool_result_restores_everywhere() {
        // 同一个值出现两次拿同一个占位符，而且两处都得原样还原 ——
        // 包括藏在工具结果里的那一处
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
            "同一个值该是同一个占位符"
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
        // 模型看到两个不同的占位符会当成两个人；直通请求也要求
        // 「值 → 占位符」是个函数
        let redactor = PiiRedactor::new();
        let mut request = ir_request(vec![ir_user_message(vec![Part::Text(
            "to a@example.com, cc a@example.com, bcc b@example.com".into(),
        )])]);
        let ctx = redactor.redact_request(&mut request);
        assert_eq!(ctx.replacements.len(), 2, "{:?}", ctx.replacements);
    }

    #[test]
    fn applying_to_a_raw_request_reaches_text_the_client_escaped() {
        // 客户端可能发 `\u0040`，字节里就没有 `@` 了。在解析后的 Value
        // 上做，看到的是解开的字符串
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
            "改掉的会是图片，不是 PII"
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
        // 原值带引号时，原样塞回去会把 JSON 弄坏
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
