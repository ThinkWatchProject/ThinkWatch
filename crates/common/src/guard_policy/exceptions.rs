//! Exception values of outbound redaction (`security.redact.exceptions`):
//! values that are never replaced or recorded, whichever rule finds them.
//!
//! They live in the redaction policy, as thinkwatch-core defines it
//! (`tw_guard::policy::RedactException`): **the SHA-256 of each value and
//! its masked form, never the value**. The value an admin pastes is hashed
//! as soon as it arrives and goes no further.
//!
//! **They are edited only through their own endpoints**
//! (`/api/admin/security/redact/exceptions`). The console writes the rest
//! of the policy back whole, rebuilt from the rule view, and the view does
//! not carry them: a policy written through the settings endpoint keeps the
//! stored exceptions ([`keep_stored`]) rather than losing them.

use serde_json::Value;
use tw_guard::policy::RedactException;

/// The exceptions a stored redaction policy holds. A policy that is
/// missing, or whose exceptions cannot be read, holds none.
pub fn stored(policy: Option<&Value>) -> Vec<RedactException> {
    policy
        .and_then(|p| p.get("exceptions"))
        .and_then(|e| serde_json::from_value(e.clone()).ok())
        .unwrap_or_default()
}

/// `policy` (a stored redaction policy, or none) with `list` as its
/// exceptions; the field is left out when the list is empty, as the
/// factory policy leaves it out. Everything else stays as stored.
pub fn with_list(policy: Option<Value>, list: &[RedactException]) -> Value {
    let mut policy = match policy {
        Some(Value::Object(map)) => Value::Object(map),
        _ => Value::Object(Default::default()),
    };
    let map = policy.as_object_mut().expect("an object, just made one");
    if list.is_empty() {
        map.remove("exceptions");
    } else {
        map.insert(
            "exceptions".into(),
            serde_json::to_value(list).expect("exceptions serialise"),
        );
    }
    policy
}

/// A redaction policy about to be saved through the settings endpoint,
/// with the exceptions `stored` holds instead of whatever it carries.
pub fn keep_stored(incoming: Value, stored_policy: Option<&Value>) -> Value {
    match incoming {
        Value::Object(_) => with_list(Some(incoming), &stored(stored_policy)),
        // Not an object: validation refuses it as it is.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(sha: char) -> RedactException {
        RedactException {
            sha256: sha.to_string().repeat(64),
            masked: "sk-an…7f9c".into(),
            rule: "anthropic-api-key".into(),
            added_at: 1,
        }
    }

    #[test]
    fn a_settings_write_keeps_the_stored_exceptions() {
        let stored_policy = with_list(Some(json!({"mode": "observe"})), &[one('a')]);
        assert_eq!(stored(Some(&stored_policy)), [one('a')]);
        // The console writes the policy without them, or with others.
        for incoming in [
            json!({"mode": "enforce"}),
            json!({"mode": "enforce", "exceptions": [one('b')]}),
            json!({"mode": "enforce", "exceptions": []}),
        ] {
            let kept = keep_stored(incoming, Some(&stored_policy));
            assert_eq!(kept["mode"], "enforce");
            assert_eq!(stored(Some(&kept)), [one('a')]);
        }
        // Nothing stored: none come in either.
        let kept = keep_stored(json!({"exceptions": [one('b')]}), None);
        assert!(kept.get("exceptions").is_none(), "{kept}");
        // Not an object: left for validation to refuse.
        assert_eq!(keep_stored(json!([]), Some(&stored_policy)), json!([]));
    }

    #[test]
    fn an_empty_list_leaves_the_field_out_and_the_rest_as_stored() {
        let p = with_list(
            Some(json!({"mode": "enforce", "disable": ["jwt"], "exceptions": [one('a')]})),
            &[],
        );
        assert_eq!(p, json!({"mode": "enforce", "disable": ["jwt"]}));
        assert_eq!(with_list(None, &[]), json!({}));
        assert!(stored(Some(&json!({"exceptions": "not a list"}))).is_empty());
    }
}
