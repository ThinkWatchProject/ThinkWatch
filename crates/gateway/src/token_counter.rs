use crate::providers::traits::ChatMessage;

/// Rough token estimation for a piece of text.
///
/// Uses a simple heuristic: ~4 characters per token for Latin/ASCII text,
/// ~2 characters per token for CJK characters. This intentionally
/// over-estimates rather than under-estimates so that rate limits are
/// conservative.
pub fn estimate_tokens(text: &str) -> u32 {
    let mut latin_chars: u32 = 0;
    let mut cjk_chars: u32 = 0;

    for ch in text.chars() {
        if is_cjk(ch) {
            cjk_chars += 1;
        } else {
            latin_chars += 1;
        }
    }

    let latin_tokens = latin_chars.div_ceil(4);
    let cjk_tokens = cjk_chars.div_ceil(2);

    latin_tokens + cjk_tokens
}

/// Sum up estimated tokens across all messages in a conversation.
///
/// Each message incurs a small fixed overhead (~4 tokens for role/framing)
/// plus the content tokens.
pub fn count_message_tokens(messages: &[ChatMessage]) -> u32 {
    let mut total: u32 = 0;

    for msg in messages {
        // ~4 tokens of overhead per message for role, delimiters, etc.
        total += 4;

        let content_text = match &msg.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        total += estimate_tokens(&content_text);
    }

    total
}

/// Returns true if the character falls in a CJK Unified Ideographs range.
fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_estimation() {
        // "hello world" = 11 chars -> ~3 tokens
        let tokens = estimate_tokens("hello world");
        assert!((2..=4).contains(&tokens), "got {tokens}");
    }

    #[test]
    fn cjk_estimation() {
        // 4 CJK chars -> ~2 tokens
        let tokens = estimate_tokens("\u{4F60}\u{597D}\u{4E16}\u{754C}");
        assert!((2..=4).contains(&tokens), "got {tokens}");
    }

    #[test]
    fn mixed_text() {
        let tokens = estimate_tokens("Hello \u{4F60}\u{597D}");
        assert!(tokens > 0);
    }

    #[test]
    fn message_tokens() {
        let messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello, how are you?".to_string()),
                ..Default::default()
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String("I am fine, thank you!".to_string()),
                ..Default::default()
            },
        ];
        let total = count_message_tokens(&messages);
        assert!(total > 8, "expected overhead + content, got {total}");
    }
}
