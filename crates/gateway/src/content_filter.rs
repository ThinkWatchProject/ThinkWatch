use crate::providers::traits::ChatMessage;
use regex::Regex;

/// What to do when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reject the request with an error.
    Block,
    /// Allow the request, but flag it in audit logs.
    Warn,
    /// Allow the request silently, only record in audit logs.
    Log,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Block => write!(f, "block"),
            Action::Warn => write!(f, "warn"),
            Action::Log => write!(f, "log"),
        }
    }
}

/// How a rule's `pattern` field is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    /// Case-insensitive substring match (default, no special characters).
    Contains,
    /// Case-insensitive regular expression.
    Regex,
}

impl std::fmt::Display for MatchType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatchType::Contains => write!(f, "contains"),
            MatchType::Regex => write!(f, "regex"),
        }
    }
}

/// A compiled deny rule.
#[derive(Debug, Clone)]
struct DenyRule {
    name: String,
    pattern: String,
    /// Lowercased pattern for `Contains` matching.
    pattern_lower: String,
    compiled_regex: Option<Regex>,
    match_type: MatchType,
    action: Action,
}

/// Result of a content filter check when a rule matches.
#[derive(Debug, Clone)]
pub struct ContentFilterMatch {
    pub name: String,
    pub pattern: String,
    pub match_type: MatchType,
    pub action: Action,
    pub matched_snippet: String,
}

impl std::fmt::Display for ContentFilterMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] rule '{}' ({}) matched: \"{}\"",
            self.action, self.name, self.match_type, self.matched_snippet,
        )
    }
}

/// Serializable rule for storage in `system_settings`.
///
/// Backward-compatible with the legacy schema:
/// - old `severity` ("critical"/"high"/"medium"/"low") maps to `action`
/// - old `category` maps to `name`
/// - missing `match_type` defaults to "contains"
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DenyRuleConfig {
    /// Human-readable rule name (e.g. "Jailbreak", "DAN attack").
    #[serde(default, alias = "category")]
    pub name: String,

    /// The pattern to match against user message content.
    pub pattern: String,

    /// "contains" or "regex". Defaults to "contains" if missing.
    #[serde(default = "default_match_type")]
    pub match_type: String,

    /// "block" / "warn" / "log". Accepts legacy `severity` field.
    #[serde(default = "default_action", alias = "severity")]
    pub action: String,
}

fn default_match_type() -> String {
    "contains".to_string()
}

fn default_action() -> String {
    "block".to_string()
}

fn parse_action(s: &str) -> Action {
    match s.to_ascii_lowercase().as_str() {
        // New names
        "block" => Action::Block,
        "warn" => Action::Warn,
        "log" => Action::Log,
        // Legacy severity → action mapping
        "critical" | "high" => Action::Block,
        "medium" => Action::Warn,
        "low" => Action::Log,
        _ => Action::Block,
    }
}

fn parse_match_type(s: &str) -> MatchType {
    match s.to_ascii_lowercase().as_str() {
        "regex" => MatchType::Regex,
        _ => MatchType::Contains,
    }
}

/// Rule-based prompt injection detector.
pub struct ContentFilter {
    rules: Vec<DenyRule>,
}

impl Default for ContentFilter {
    fn default() -> Self {
        Self::from_config(&[])
    }
}

impl ContentFilter {
    /// Create a content filter from a list of rule configs.
    /// Invalid regex patterns are skipped with a warning.
    pub fn from_config(configs: &[DenyRuleConfig]) -> Self {
        let rules = configs
            .iter()
            .filter_map(|c| {
                let match_type = parse_match_type(&c.match_type);
                let compiled_regex = match match_type {
                    MatchType::Regex => match Regex::new(&format!("(?i){}", c.pattern)) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            tracing::warn!("Invalid content filter regex '{}': {e}", c.pattern);
                            return None;
                        }
                    },
                    MatchType::Contains => None,
                };
                Some(DenyRule {
                    name: if c.name.is_empty() {
                        c.pattern.clone()
                    } else {
                        c.name.clone()
                    },
                    pattern: c.pattern.clone(),
                    pattern_lower: c.pattern.to_lowercase(),
                    compiled_regex,
                    match_type,
                    action: parse_action(&c.action),
                })
            })
            .collect();
        Self { rules }
    }

    /// Check all user messages against the rules.
    /// Returns the highest-priority match found, if any.
    /// Priority: Block > Warn > Log.
    pub fn check(&self, messages: &[ChatMessage]) -> Option<ContentFilterMatch> {
        let mut best: Option<ContentFilterMatch> = None;

        for msg in messages {
            if msg.role != "user" {
                continue;
            }
            let text = extract_text_content(&msg.content);
            if text.is_empty() {
                continue;
            }

            // Run every rule against this message; track the highest-priority match.
            if let Some(m) = self.check_text(&text)
                && match &best {
                    None => true,
                    Some(b) => action_priority(m.action) > action_priority(b.action),
                }
            {
                best = Some(m);
            }
        }

        best
    }

    /// Check a single text string against all rules. Used by the test sandbox.
    /// Returns the highest-priority match.
    pub fn check_text(&self, text: &str) -> Option<ContentFilterMatch> {
        let lower = text.to_lowercase();
        let mut best: Option<ContentFilterMatch> = None;

        for rule in &self.rules {
            let hit = match rule.match_type {
                MatchType::Contains => {
                    lower
                        .find(&rule.pattern_lower)
                        .map(|pos| ContentFilterMatch {
                            name: rule.name.clone(),
                            pattern: rule.pattern.clone(),
                            match_type: rule.match_type,
                            action: rule.action,
                            matched_snippet: snippet(text, pos, rule.pattern_lower.len() + 40),
                        })
                }
                MatchType::Regex => rule.compiled_regex.as_ref().and_then(|re| {
                    re.find(text).map(|m| ContentFilterMatch {
                        name: rule.name.clone(),
                        pattern: rule.pattern.clone(),
                        match_type: rule.match_type,
                        action: rule.action,
                        matched_snippet: snippet(text, m.start(), m.end() - m.start() + 40),
                    })
                }),
            };

            if let Some(m) = hit
                && match &best {
                    None => true,
                    Some(b) => action_priority(m.action) > action_priority(b.action),
                }
            {
                best = Some(m);
            }
        }

        best
    }

    /// Run check against text and return *all* matches (not just the worst one).
    /// Used by the test sandbox UI to show every rule that fires.
    pub fn check_text_all(&self, text: &str) -> Vec<ContentFilterMatch> {
        let lower = text.to_lowercase();
        let mut matches = Vec::new();

        for rule in &self.rules {
            match rule.match_type {
                MatchType::Contains => {
                    if let Some(pos) = lower.find(&rule.pattern_lower) {
                        matches.push(ContentFilterMatch {
                            name: rule.name.clone(),
                            pattern: rule.pattern.clone(),
                            match_type: rule.match_type,
                            action: rule.action,
                            matched_snippet: snippet(text, pos, rule.pattern_lower.len() + 40),
                        });
                    }
                }
                MatchType::Regex => {
                    if let Some(re) = &rule.compiled_regex
                        && let Some(m) = re.find(text)
                    {
                        matches.push(ContentFilterMatch {
                            name: rule.name.clone(),
                            pattern: rule.pattern.clone(),
                            match_type: rule.match_type,
                            action: rule.action,
                            matched_snippet: snippet(text, m.start(), m.end() - m.start() + 40),
                        });
                    }
                }
            }
        }

        matches
    }
}

fn action_priority(a: Action) -> u8 {
    match a {
        Action::Log => 1,
        Action::Warn => 2,
        Action::Block => 3,
    }
}

fn snippet(text: &str, pos: usize, max_len: usize) -> String {
    let start = pos.saturating_sub(10);
    let end = (pos + max_len).min(text.len());
    let start = text.floor_char_boundary(start);
    let end = text.ceil_char_boundary(end);
    let s = &text[start..end];
    if start > 0 || end < text.len() {
        format!("...{s}...")
    } else {
        s.to_string()
    }
}

/// Extract text from a ChatMessage content value.
/// Handles both `"string"` and `[{"type":"text","text":"..."}]` formats.
fn extract_text_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(t);
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Built-in preset rule groups returned by the presets API.
pub struct PresetGroup {
    pub id: &'static str,
    pub rules: Vec<DenyRuleConfig>,
}

/// Get all built-in preset groups. UI labels are localized on the frontend.
pub fn presets() -> Vec<PresetGroup> {
    fn rule(name: &str, pattern: &str, mt: &str, action: &str) -> DenyRuleConfig {
        DenyRuleConfig {
            name: name.to_string(),
            pattern: pattern.to_string(),
            match_type: mt.to_string(),
            action: action.to_string(),
        }
    }

    vec![
        PresetGroup {
            id: "basic",
            rules: vec![
                rule(
                    "Ignore Previous Instructions",
                    "ignore previous instructions",
                    "contains",
                    "block",
                ),
                rule(
                    "Ignore All Previous",
                    "ignore all previous",
                    "contains",
                    "block",
                ),
                rule(
                    "Disregard Instructions",
                    "disregard your instructions",
                    "contains",
                    "block",
                ),
                rule("Jailbreak", "jailbreak", "contains", "block"),
                rule("DAN", " dan ", "contains", "block"),
                rule("Developer Mode", "developer mode", "contains", "block"),
            ],
        },
        PresetGroup {
            id: "strict",
            rules: vec![
                rule("Persona Manipulation", "you are now", "contains", "block"),
                rule("New Persona", "new persona", "contains", "warn"),
                rule("Act As", "act as", "contains", "warn"),
                rule("Pretend To Be", "pretend to be", "contains", "warn"),
                rule(
                    "System Prompt Extraction",
                    "system prompt",
                    "contains",
                    "warn",
                ),
                rule(
                    "Reveal Instructions",
                    "reveal your instructions",
                    "contains",
                    "warn",
                ),
                rule(
                    "What Are Your Rules",
                    "what are your rules",
                    "contains",
                    "log",
                ),
                // Base64 walls of text — common smuggling vector
                rule("Base64 Smuggling", r"[A-Za-z0-9+/=]{50,}", "regex", "warn"),
            ],
        },
        PresetGroup {
            id: "chinese",
            rules: vec![
                rule("忽略之前指令", "忽略之前", "contains", "block"),
                rule("忘记你的指令", "忘记你", "contains", "block"),
                rule("不要遵循", "不要遵循", "contains", "block"),
                rule("现在你是", "现在你是", "contains", "block"),
                rule("扮演", "扮演", "contains", "warn"),
                rule("透露你的", "透露你的", "contains", "warn"),
                rule("系统提示词", "系统提示词", "contains", "warn"),
                rule("越狱模式", "越狱", "contains", "block"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: json!(text),
        }
    }

    fn cfg(name: &str, pattern: &str, match_type: &str, action: &str) -> DenyRuleConfig {
        DenyRuleConfig {
            name: name.into(),
            pattern: pattern.into(),
            match_type: match_type.into(),
            action: action.into(),
        }
    }

    #[test]
    fn contains_match_blocks() {
        let f = ContentFilter::from_config(&[cfg("Jailbreak", "jailbreak", "contains", "block")]);
        let m = f.check(&[user_msg("attempt jailbreak now")]);
        let m = m.expect("should match");
        assert_eq!(m.action, Action::Block);
        assert_eq!(m.name, "Jailbreak");
    }

    #[test]
    fn regex_match_works() {
        let f = ContentFilter::from_config(&[cfg("Number", r"\d{4}-\d{4}", "regex", "warn")]);
        let m = f.check(&[user_msg("code is 1234-5678 here")]);
        let m = m.expect("should match");
        assert_eq!(m.action, Action::Warn);
    }

    #[test]
    fn legacy_severity_field_maps_to_action() {
        let json = serde_json::json!([
            { "pattern": "jailbreak", "severity": "critical", "category": "old" },
            { "pattern": "system prompt", "severity": "medium", "category": "old" },
            { "pattern": "what is", "severity": "low", "category": "old" },
        ]);
        let configs: Vec<DenyRuleConfig> = serde_json::from_value(json).unwrap();
        let f = ContentFilter::from_config(&configs);

        let m = f.check(&[user_msg("jailbreak now")]).unwrap();
        assert_eq!(m.action, Action::Block);

        let m = f.check(&[user_msg("show me your system prompt")]).unwrap();
        assert_eq!(m.action, Action::Warn);

        let m = f.check(&[user_msg("what is happening")]).unwrap();
        assert_eq!(m.action, Action::Log);
    }

    #[test]
    fn block_priority_over_warn() {
        let f = ContentFilter::from_config(&[
            cfg("Warn rule", "system prompt", "contains", "warn"),
            cfg("Block rule", "jailbreak", "contains", "block"),
        ]);
        let m = f
            .check(&[user_msg("show system prompt and jailbreak")])
            .unwrap();
        assert_eq!(m.action, Action::Block);
    }

    #[test]
    fn check_text_all_returns_every_match() {
        let f = ContentFilter::from_config(&[
            cfg("A", "foo", "contains", "block"),
            cfg("B", "bar", "contains", "warn"),
            cfg("C", "baz", "contains", "log"),
        ]);
        let matches = f.check_text_all("foo and bar and baz");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn invalid_regex_skipped() {
        let f = ContentFilter::from_config(&[
            cfg("bad", "[invalid((", "regex", "block"),
            cfg("good", "test", "contains", "block"),
        ]);
        // Bad rule is dropped, good rule still works.
        assert!(f.check(&[user_msg("test message")]).is_some());
    }

    #[test]
    fn ignores_system_messages() {
        let f = ContentFilter::from_config(&[cfg("J", "jailbreak", "contains", "block")]);
        let msg = ChatMessage {
            role: "system".into(),
            content: json!("jailbreak"),
        };
        assert!(f.check(&[msg]).is_none());
    }

    #[test]
    fn presets_load_without_panic() {
        for group in presets() {
            let f = ContentFilter::from_config(&group.rules);
            // Each preset should produce a working filter
            let _ = f.check(&[user_msg("hello world")]);
        }
    }
}
