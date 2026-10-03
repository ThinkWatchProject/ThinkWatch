//! The request guards' policies, as system settings.
//!
//! Three guards run on every request through the AI gateway: outbound
//! redaction (`security.redact`), tool-call inspection
//! (`security.inspect_tools`) and the content filter
//! (`security.content`). Each key holds one guard's whole policy as a
//! JSON object.
//!
//! **The shape is thinkwatch-core's** (`tw_guard::policy`), and so are its
//! factory values, its validation, the rule view the console shows
//! (`tw_guard::view`) and the sample trial (`tw_guard::trial`): the
//! desktop gateway keeps the same JSON under `security:` in its config
//! file. What belongs to this side is where the value is stored, how a bad
//! one is reported (a 400 on save, a loud log at runtime), and the
//! one-time conversion of the settings this gateway kept before
//! ([`legacy`]).
//!
//! An untouched policy is `{}`: every field left out is the factory value,
//! and the factory mode is observe — every hit is recorded, nothing on the
//! wire changes.

use serde::de::DeserializeOwned;
use serde_json::Value;
use tw_guard::policy::{ContentPolicy, Guard, RedactPolicy, Security, ToolPolicy};
use tw_guard::redact::rules::RuleSet;

use crate::dynamic_config::DynamicConfig;

pub mod legacy;

/// The settings key a guard's policy is stored under.
pub fn key(guard: Guard) -> &'static str {
    match guard {
        Guard::Redact => "security.redact",
        Guard::InspectTools => "security.inspect_tools",
        Guard::Content => "security.content",
    }
}

/// The guard whose policy `key` holds, if it holds one.
pub fn guard_of(key: &str) -> Option<Guard> {
    Guard::ALL.iter().copied().find(|g| self::key(*g) == key)
}

/// Check a value for one guard's key before it is saved: its shape (a
/// misspelt field is an error, not a silent factory value) and its rules
/// (unknown built-in ids, patterns that do not compile, malformed code
/// points or placeholder names, custom rules without a name or sharing
/// one). The error is the sentence the admin sees.
pub fn validate(guard: Guard, value: &Value) -> Result<(), String> {
    let checked = match guard {
        Guard::Redact => parse::<RedactPolicy>(guard, value)?.check(),
        Guard::InspectTools => parse::<ToolPolicy>(guard, value)?.check(),
        Guard::Content => parse::<ContentPolicy>(guard, value)?.check(),
    };
    checked.map_err(|e| e.to_string())
}

fn parse<T: DeserializeOwned>(guard: Guard, value: &Value) -> Result<T, String> {
    // serde would read a struct out of a JSON array too (`[]` as every
    // field left out); a policy is an object.
    if !value.is_object() {
        return Err(format!("{}: expected a JSON object", key(guard)));
    }
    serde_json::from_value(value.clone()).map_err(|e| format!("{}: {e}", key(guard)))
}

/// The three policies as stored.
///
/// A key that is missing is the factory policy. **One that cannot be read
/// is the factory policy too, loudly**: the settings endpoint refuses
/// such a value, so one here was written around it, and refusing to
/// start over it would take the whole gateway down for one guard.
pub async fn read(dc: &DynamicConfig) -> Security {
    Security {
        redact: read_one(dc, Guard::Redact).await,
        inspect_tools: read_one(dc, Guard::InspectTools).await,
        content: read_one(dc, Guard::Content).await,
    }
}

async fn read_one<T: DeserializeOwned + Default>(dc: &DynamicConfig, guard: Guard) -> T {
    let Some(value) = dc.get(key(guard)).await else {
        return T::default();
    };
    serde_json::from_value(value).unwrap_or_else(|e| {
        tracing::error!(
            key = key(guard),
            error = %e,
            "Unreadable guard policy — running this guard on its factory policy"
        );
        metrics::counter!("guard_policy_unreadable_total", "guard" => guard.slug()).increment(1);
        T::default()
    })
}

// ---------------------------------------------------------------- compile
//
// **A custom rule that does not compile is dropped, loudly, and the rest
// still run.** The settings endpoint refuses such a policy, so one here was
// written around it; dropping the whole policy would switch the guard off
// over one rule.

/// The outbound redaction rules a policy runs.
pub fn redact_rules(policy: &RedactPolicy) -> RuleSet {
    policy.rules().unwrap_or_else(|e| {
        skipped(Guard::Redact, &e);
        let mut kept = policy.clone();
        kept.custom.retain(|c| {
            RedactPolicy {
                custom: vec![c.clone()],
                ..Default::default()
            }
            .check()
            .is_ok()
        });
        kept.rules().unwrap_or_else(|_| RuleSet::none())
    })
}

/// The tool-call inspection rules a policy runs.
pub fn tool_rules(policy: &ToolPolicy) -> tw_guard::tools::rules::Rules {
    policy.rules().unwrap_or_else(|e| {
        skipped(Guard::InspectTools, &e);
        let mut kept = policy.clone();
        kept.custom.retain(|c| {
            ToolPolicy {
                custom: vec![c.clone()],
                ..Default::default()
            }
            .check()
            .is_ok()
        });
        kept.rules()
            .unwrap_or_else(|_| tw_guard::tools::rules::Rules { rules: Vec::new() })
    })
}

/// The content filter rules a policy runs.
pub fn content_rules(policy: &ContentPolicy) -> tw_guard::content::Rules {
    policy.rules().unwrap_or_else(|e| {
        skipped(Guard::Content, &e);
        let mut kept = policy.clone();
        kept.custom.retain(|c| {
            ContentPolicy {
                custom: vec![c.clone()],
                ..Default::default()
            }
            .check()
            .is_ok()
        });
        kept.rules()
            .unwrap_or_else(|_| tw_guard::content::Rules::none())
    })
}

fn skipped(guard: Guard, e: &tw_guard::policy::PolicyError) {
    tracing::error!(
        key = key(guard),
        error = %e,
        "Guard policy does not compile — custom rules that do not are DISABLED"
    );
    metrics::counter!("guard_policy_invalid_total", "guard" => guard.slug()).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_guard_has_its_own_key() {
        for &g in Guard::ALL {
            assert_eq!(guard_of(key(g)), Some(g));
        }
        assert_eq!(guard_of("security.hidden_text"), None);
    }

    #[test]
    fn the_factory_policy_is_an_empty_object() {
        for &g in Guard::ALL {
            assert_eq!(validate(g, &json!({})), Ok(()), "{g}");
        }
        assert_eq!(
            serde_json::to_value(Security::default().content).unwrap(),
            json!({})
        );
    }

    #[test]
    fn a_misspelt_field_or_a_bad_rule_is_refused() {
        let e = validate(Guard::Redact, &json!({"mode": "observ"})).unwrap_err();
        assert!(e.starts_with("security.redact: "), "{e}");
        let e = validate(Guard::Content, &json!({"hidden_text": "block"})).unwrap_err();
        assert!(e.contains("hidden_text"), "{e}");
        let e = validate(
            Guard::Content,
            &json!({"custom": [{"name": "a", "pattern": "U+GG", "match": "codepoints"}]}),
        )
        .unwrap_err();
        assert!(e.contains("code points"), "{e}");
        let e = validate(Guard::InspectTools, &json!({"disable": ["no-such-rule"]})).unwrap_err();
        assert!(e.contains("no-such-rule"), "{e}");
        let e = validate(
            Guard::Redact,
            &json!({"custom": [{"name": "a", "pattern": "x", "label": "lower"}]}),
        )
        .unwrap_err();
        assert!(e.contains("placeholder name"), "{e}");
    }

    #[test]
    fn a_custom_rule_that_does_not_compile_is_dropped_and_the_rest_run() {
        let content: ContentPolicy = serde_json::from_value(json!({"custom": [
            {"name": "bad", "pattern": "[", "match": "regex", "action": "block"},
            {"name": "good", "pattern": "fine", "action": "block"}
        ]}))
        .unwrap();
        let ids: Vec<String> = content_rules(&content)
            .rules
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(ids.contains(&"good".to_string()), "{ids:?}");
        assert!(!ids.contains(&"bad".to_string()), "{ids:?}");
        assert!(
            ids.contains(&"unicode-tags".to_string()),
            "built-in rules still run"
        );

        let tools: ToolPolicy = serde_json::from_value(json!({"custom": [
            {"name": "bad", "pattern": "("},
            {"name": "good", "pattern": "kubectl\\s+delete"}
        ]}))
        .unwrap();
        let ids: Vec<String> = tool_rules(&tools).rules.into_iter().map(|r| r.id).collect();
        assert!(ids.contains(&"good".to_string()) && !ids.contains(&"bad".to_string()));
        assert!(ids.contains(&"curl-pipe-sh".to_string()));

        let redact: RedactPolicy = serde_json::from_value(json!({"custom": [
            {"name": "bad", "pattern": "("},
            {"name": "good", "pattern": "PRJ-\\d+"}
        ]}))
        .unwrap();
        let rules = redact_rules(&redact);
        let hits = tw_guard::redact::rules::scan_plain("PRJ-12 (", &rules);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule.id(), "good");
    }
}
