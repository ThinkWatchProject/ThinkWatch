//! Who is calling, carried beside the request rather than inside it.

use std::collections::HashMap;

/// Per-call metadata that is *not* part of the request payload: caller
/// identity for header-template substitution, plus the trace id that
/// correlates the downstream request, the gateway log and the upstream
/// log line.
///
/// `attrs` is an open dictionary rather than named fields on purpose:
/// the substitution engine only does `{{key}}` → value and does not
/// understand what any key means. Adding `{{team_id}}` to a header
/// template becomes a caller-side change, not a signature change here.
#[derive(Debug, Clone, Default)]
pub struct CallCtx {
    /// Forwarded upstream as `x-trace-id` when present (OBS-01).
    pub trace_id: Option<String>,
    /// Values for `{{...}}` placeholders in custom header templates.
    /// Conventional keys: `user_id`, `user_email`.
    pub attrs: HashMap<String, String>,
}

impl CallCtx {
    /// Convenience for the common enterprise case: caller identity plus
    /// a trace id. Empty/absent values are simply not inserted, so a
    /// template referencing a missing key resolves to the empty string
    /// (the previous behaviour).
    pub fn new(
        trace_id: Option<String>,
        user_id: Option<String>,
        user_email: Option<String>,
    ) -> Self {
        let mut attrs = HashMap::new();
        if let Some(v) = user_id {
            attrs.insert("user_id".to_string(), v);
        }
        if let Some(v) = user_email {
            attrs.insert("user_email".to_string(), v);
        }
        Self { trace_id, attrs }
    }
}

/// Replace every `{{key}}` occurrence in `template` with `attrs[key]`,
/// or with the empty string when the key is absent.
pub fn substitute_template(template: &str, attrs: &HashMap<String, String>) -> String {
    // Fast path: most header values carry no placeholder at all.
    if !template.contains("{{") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                if let Some(v) = attrs.get(key) {
                    out.push_str(v);
                }
                rest = &after[end + 2..];
            }
            // Unterminated `{{` — emit the rest verbatim rather than
            // silently truncating a header value.
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_known_keys() {
        let a = attrs(&[("user_id", "u1"), ("user_email", "a@b.c")]);
        assert_eq!(substitute_template("{{user_id}}", &a), "u1");
        assert_eq!(
            substitute_template("id={{user_id}};mail={{user_email}}", &a),
            "id=u1;mail=a@b.c"
        );
    }

    #[test]
    fn missing_key_becomes_empty_not_literal() {
        // The old implementation had the same behaviour via `unwrap_or("")`.
        // Keeping it: a literal `{{user_id}}` reaching the upstream looks
        // like a working config and is harder to diagnose than a blank.
        assert_eq!(substitute_template("{{nope}}", &attrs(&[])), "");
        assert_eq!(substitute_template("x{{nope}}y", &attrs(&[])), "xy");
    }

    #[test]
    fn passes_through_values_without_placeholders() {
        let a = attrs(&[("user_id", "u1")]);
        assert_eq!(substitute_template("plain", &a), "plain");
        assert_eq!(substitute_template("", &a), "");
    }

    #[test]
    fn unterminated_placeholder_is_kept_verbatim() {
        // Truncating here would silently shorten a header value.
        assert_eq!(substitute_template("a{{user", &attrs(&[])), "a{{user");
    }

    #[test]
    fn new_skips_absent_identity() {
        let ctx = CallCtx::new(Some("t1".into()), None, Some("a@b.c".into()));
        assert_eq!(ctx.trace_id.as_deref(), Some("t1"));
        assert!(!ctx.attrs.contains_key("user_id"));
        assert_eq!(
            ctx.attrs.get("user_email").map(String::as_str),
            Some("a@b.c")
        );
    }
}
