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
//! wire changes. A policy saved before rules had an action of their own
//! (redaction), a scope, a case rule or a replacement (content) reads as
//! it always did: redaction rules replace with placeholders, content rules
//! check requests only and ignore case.

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

/// The longest replacement text a content rule may write, in characters.
/// It goes into every request or answer the rule matches.
pub const REPLACEMENT_CHARS: usize = 200;

/// How many custom rules a policy may hold, and how long one's pattern
/// may be, in characters. Every rule runs on every request: a policy past
/// these is a mistake, or an attempt to slow the gateway down, rather than
/// a configuration. The limits this gateway had before the guards were
/// unified.
pub struct Limits {
    pub custom_rules: usize,
    pub pattern_chars: usize,
}

/// The limits of one guard's policy.
pub fn limits(guard: Guard) -> Limits {
    match guard {
        Guard::Redact => Limits {
            custom_rules: 100,
            pattern_chars: 1000,
        },
        Guard::InspectTools => Limits {
            custom_rules: 100,
            pattern_chars: 1000,
        },
        Guard::Content => Limits {
            custom_rules: 500,
            pattern_chars: 500,
        },
    }
}

/// Check a value for one guard's key before it is saved: its shape (a
/// misspelt field is an error, not a silent factory value), its size
/// ([`limits`]; a custom tool-call rule may not take a built-in rule's
/// name either), and its rules (unknown built-in ids, patterns that do not
/// compile, malformed code points or placeholder names, custom rules
/// without a name or sharing one). The error is the sentence the admin
/// sees.
pub fn validate(guard: Guard, value: &Value) -> Result<(), String> {
    let checked = match guard {
        Guard::Redact => {
            let p = parse::<RedactPolicy>(guard, value)?;
            within_limits(guard, p.custom.iter().map(|c| (&c.name, &c.pattern)))?;
            p.check()
        }
        Guard::InspectTools => {
            let p = parse::<ToolPolicy>(guard, value)?;
            within_limits(guard, p.custom.iter().map(|c| (&c.name, &c.pattern)))?;
            let builtin = &tw_guard::tools::rules::builtin().dangerous;
            if let Some(c) = p
                .custom
                .iter()
                .find(|c| builtin.iter().any(|b| b.id == c.name))
            {
                return Err(format!(
                    "{}: custom rule `{}` has the name of a built-in rule; give it another name",
                    key(guard),
                    c.name
                ));
            }
            p.check()
        }
        Guard::Content => {
            let p = parse::<ContentPolicy>(guard, value)?;
            within_limits(guard, p.custom.iter().map(|c| (&c.name, &c.pattern)))?;
            if let Some(c) = p
                .custom
                .iter()
                .find(|c| c.replacement.chars().count() > REPLACEMENT_CHARS)
            {
                return Err(format!(
                    "{}: the replacement text of custom rule `{}` is {} characters long; at \
                     most {REPLACEMENT_CHARS} are allowed",
                    key(guard),
                    c.name,
                    c.replacement.chars().count()
                ));
            }
            p.check()
        }
    };
    checked.map_err(|e| e.to_string())
}

/// Custom rules no more and no longer than [`limits`] allows.
fn within_limits<'a>(
    guard: Guard,
    custom: impl ExactSizeIterator<Item = (&'a String, &'a String)>,
) -> Result<(), String> {
    let Limits {
        custom_rules,
        pattern_chars,
    } = limits(guard);
    if custom.len() > custom_rules {
        return Err(format!(
            "{}: {} custom rules; at most {custom_rules} are allowed",
            key(guard),
            custom.len()
        ));
    }
    for (name, pattern) in custom {
        let n = pattern.chars().count();
        if n > pattern_chars {
            return Err(format!(
                "{}: the pattern of custom rule `{name}` is {n} characters long; at most \
                 {pattern_chars} are allowed",
                key(guard)
            ));
        }
    }
    Ok(())
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
    fn a_policy_past_the_limits_is_refused_with_the_reason() {
        let rules = |n: usize, pattern: &str| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"name": format!("r{i}"), "pattern": pattern}))
                .collect()
        };
        // At the limit: fine.
        for (g, n) in [
            (Guard::Content, 500),
            (Guard::Redact, 100),
            (Guard::InspectTools, 100),
        ] {
            assert_eq!(
                validate(g, &json!({"custom": rules(n, "x")})),
                Ok(()),
                "{g}"
            );
            let e = validate(g, &json!({"custom": rules(n + 1, "x")})).unwrap_err();
            assert!(e.contains(&format!("at most {n}")), "{g}: {e}");
        }
        for (g, n) in [
            (Guard::Content, 500),
            (Guard::Redact, 1000),
            (Guard::InspectTools, 1000),
        ] {
            let long = "a".repeat(n);
            assert_eq!(
                validate(g, &json!({"custom": rules(1, &long)})),
                Ok(()),
                "{g}"
            );
            let longer = "a".repeat(n + 1);
            let e = validate(g, &json!({"custom": rules(1, &longer)})).unwrap_err();
            assert!(
                e.contains(&format!("{} characters long", n + 1)),
                "{g}: {e}"
            );
        }
        // Characters, not bytes.
        let cjk = "字".repeat(500);
        assert_eq!(
            validate(Guard::Content, &json!({"custom": rules(1, &cjk)})),
            Ok(())
        );
        let e = validate(
            Guard::InspectTools,
            &json!({"custom": [{"name": "curl-pipe-sh", "pattern": "curl"}]}),
        )
        .unwrap_err();
        assert!(e.contains("name of a built-in rule"), "{e}");
    }

    #[test]
    fn a_policy_saved_before_the_new_fields_reads_as_it_always_did() {
        // As an earlier console wrote them: no actions for redaction, no
        // scope, case rule or replacement for content.
        let content = json!({"mode": "enforce", "custom": [
            {"name": "Code", "pattern": "project-x", "action": "strip"}
        ]});
        assert_eq!(validate(Guard::Content, &content), Ok(()));
        let p: ContentPolicy = serde_json::from_value(content.clone()).unwrap();
        let c = &p.custom[0];
        assert!(c.checks_requests() && !c.checks_answers());
        assert!(!c.case_sensitive);
        assert_eq!(c.replacement, tw_guard::policy::DEFAULT_REPLACEMENT);
        // Written back, the defaults stay out: the stored value is unchanged.
        assert_eq!(serde_json::to_value(&p).unwrap(), content);
        let rules = content_rules(&p);
        let r = rules.rules.iter().find(|r| r.id == "Code").unwrap();
        assert!(r.sides.request && !r.sides.response);
        assert!(!r.case_sensitive);

        let redact = json!({"mode": "enforce", "custom": [
            {"name": "Project", "pattern": "PRJ-\\d+", "match": "regex", "label": "PROJECT"}
        ]});
        assert_eq!(validate(Guard::Redact, &redact), Ok(()));
        let p: RedactPolicy = serde_json::from_value(redact.clone()).unwrap();
        assert_eq!(
            p.custom[0].action,
            tw_guard::policy::RedactAction::Placeholder
        );
        assert!(p.actions.is_empty());
        assert_eq!(serde_json::to_value(&p).unwrap(), redact);
        let hits = tw_guard::redact::rules::scan_plain("PRJ-12", &redact_rules(&p));
        assert!(
            !redact_rules(&p).records(&hits[0].rule),
            "replaced, as before"
        );
    }

    #[test]
    fn the_new_fields_are_read_and_checked() {
        let content = json!({"mode": "enforce", "custom": [{
            "name": "Codename", "pattern": "Polaris", "scope": ["request", "response"],
            "case_sensitive": true, "action": "replace", "replacement": "[hidden]"
        }]});
        assert_eq!(validate(Guard::Content, &content), Ok(()));
        let p: ContentPolicy = serde_json::from_value(content).unwrap();
        let r = &content_rules(&p).rules;
        let r = r.iter().find(|r| r.id == "Codename").unwrap();
        assert!(r.sides.request && r.sides.response && r.case_sensitive);
        assert_eq!(r.replacement, "[hidden]");

        // An answer cannot be refused, nor matched by code points.
        let e = validate(
            Guard::Content,
            &json!({"custom": [{"name": "a", "pattern": "x", "scope": ["response"], "action": "block"}]}),
        )
        .unwrap_err();
        assert!(e.contains("checks answers and blocks"), "{e}");
        let e = validate(
            Guard::Content,
            &json!({"custom": [{"name": "a", "pattern": "U+200B", "match": "codepoints",
                                 "scope": ["request", "response"]}]}),
        )
        .unwrap_err();
        assert!(e.contains("code points") || e.contains("codepoints"), "{e}");
        let e = validate(
            Guard::Content,
            &json!({"custom": [{"name": "a", "pattern": "x", "scope": ["answer"]}]}),
        )
        .unwrap_err();
        assert!(e.contains("answer"), "{e}");
        let e = validate(
            Guard::Content,
            &json!({"custom": [{"name": "a", "pattern": "x", "action": "replace",
                                 "replacement": "r".repeat(REPLACEMENT_CHARS + 1)}]}),
        )
        .unwrap_err();
        assert!(e.contains("replacement text"), "{e}");
        assert_eq!(
            validate(
                Guard::Content,
                &json!({"custom": [{"name": "a", "pattern": "x", "action": "replace",
                                     "replacement": "字".repeat(REPLACEMENT_CHARS)}]}),
            ),
            Ok(())
        );

        // Redaction: built-in actions by id, and a custom rule that records.
        let redact = json!({"mode": "enforce", "actions": {"email": "record"}, "custom": [
            {"name": "Project", "pattern": "PRJ-", "action": "record"}
        ]});
        assert_eq!(validate(Guard::Redact, &redact), Ok(()));
        let p: RedactPolicy = serde_json::from_value(redact).unwrap();
        let rules = redact_rules(&p);
        let hits = tw_guard::redact::rules::scan_plain("PRJ-1", &rules);
        assert!(rules.records(&hits[0].rule));
        let e = validate(
            Guard::Redact,
            &json!({"actions": {"no-such-rule": "record"}}),
        )
        .unwrap_err();
        assert!(e.contains("no-such-rule"), "{e}");
        let e = validate(Guard::Redact, &json!({"actions": {"email": "replace"}})).unwrap_err();
        assert!(e.starts_with("security.redact: "), "{e}");
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
            {"name": "bad", "pattern": "(", "match": "regex"},
            {"name": "good", "pattern": "PRJ-\\d+", "match": "regex"}
        ]}))
        .unwrap();
        let rules = redact_rules(&redact);
        let hits = tw_guard::redact::rules::scan_plain("PRJ-12 (", &rules);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule.id(), "good");
    }
}
