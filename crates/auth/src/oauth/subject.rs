//! Pure JWT / userinfo subject-extraction helpers. Given an OAuth
//! access token (opaque or JWT) or a `userinfo` JSON response, find
//! the best human-meaningful identifier to stamp into
//! `mcp_user_credentials.upstream_subject` / display in the UI.
//!
//! The HTTP glue that picks "JWT first, then userinfo endpoint,
//! else fall through to NULL" lives in the handler crate — those
//! are async + need a reqwest client. This module owns the *pure*
//! pieces:
//!
//! - [`subject_from_jwt`] — best-effort decode of the JWT
//!   middle segment, then `extract_subject_from_json`.
//! - [`extract_subject_from_json`] — walks an object looking for
//!   the first non-empty subject-like field across a stable
//!   priority list, descending one level into known wrapper keys
//!   (`user`, `data`, `account`, `results`).
//!
//! Test coverage is co-located so adding a new provider shape is
//! "add a known JSON snippet + assertion" right here.

/// Field names the resolver searches for, in priority order. The list
/// covers OIDC standard claims first, then the conventions every major
/// non-OIDC public OAuth provider has converged on:
///
/// - `sub` / `preferred_username` / `email`: OIDC standard claims.
/// - `accountId`: Atlassian (Jira, Confluence) `/me` response.
/// - `login`: GitHub `/user` response.
/// - `username` / `name`: Slack `users.identity`, Discord `/users/@me`.
/// - `id`: Notion `/v1/users/me`, Cloudflare `/user`, generic JSON:API.
///
/// `email` is the last resort because some providers stuff the *user's
/// own* email at top-level (good signal) but others embed an *org*
/// email under a different node (bad signal); putting it last means
/// the more specific `id`-flavoured fields win when both are present.
const SUBJECT_KEYS: &[&str] = &[
    "preferred_username",
    "sub",
    "accountId",
    "login",
    "username",
    "name",
    "id",
    "email",
];

/// JSON node names the resolver descends into when no top-level key
/// matches. Matches the shape Slack returns (`{ "user": { ... } }`)
/// and the JSON:API convention (`{ "data": { ... } }`). Limited to
/// ONE level of recursion so we don't accidentally surface a nested
/// org / team identifier as the user's identity.
const NESTED_WRAPPERS: &[&str] = &["user", "data", "account", "results"];

/// Try to read a subject claim out of an access_token's JWT payload.
/// Returns `None` for opaque tokens (anything that isn't 3 dot-
/// separated base64url segments whose middle segment decodes to a
/// JSON object).
pub fn subject_from_jwt(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_bytes = data_encoding::BASE64URL_NOPAD
        .decode(parts[1].as_bytes())
        // Some encoders include `=` padding even though RFC 7515
        // forbids it. Trim and retry with the no-pad decoder before
        // giving up. (The original code passed to BASE64URL — the
        // padded variant — after stripping `=`, which always
        // failed; that bug never had a test exercising it.)
        .or_else(|_| {
            data_encoding::BASE64URL_NOPAD.decode(parts[1].trim_end_matches('=').as_bytes())
        })
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    extract_subject_from_json(&payload)
}

/// Walk a JSON object looking for the first non-empty subject-like
/// field, preferring top-level keys over nested wrappers and
/// preferring more-specific names (`preferred_username`) over less
/// (`email`).
pub fn extract_subject_from_json(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    // Pass 1: prefer top-level matches in priority order.
    for key in SUBJECT_KEYS {
        if let Some(s) = obj.get(*key).and_then(stringify_subject) {
            return Some(s);
        }
    }
    // Pass 2: descend into a recognised wrapper. Only one level — we
    // don't want to surface a deeply-nested team/org identifier as
    // the user's identity.
    for wrapper in NESTED_WRAPPERS {
        let Some(inner_v) = obj.get(*wrapper) else {
            continue;
        };
        // `results` (JSON:API) wraps an array; take the first element.
        let inner = match inner_v {
            serde_json::Value::Array(items) => items.first()?,
            other => other,
        };
        let Some(inner_obj) = inner.as_object() else {
            continue;
        };
        for key in SUBJECT_KEYS {
            if let Some(s) = inner_obj.get(*key).and_then(stringify_subject) {
                return Some(s);
            }
        }
    }
    None
}

/// Coerce a JSON value into the string we'd display to the user.
/// Strings pass through unchanged; integers (some upstreams return
/// `id` as a number) get stringified; everything else is rejected so
/// we never paint `null` / `false` / `[]` into the UI.
fn stringify_subject(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_prefers_preferred_username_over_email() {
        let v = json!({
            "preferred_username": "alice",
            "email": "alice@example.com",
        });
        assert_eq!(extract_subject_from_json(&v), Some("alice".to_owned()));
    }

    #[test]
    fn extract_prefers_sub_over_id() {
        let v = json!({ "sub": "12345", "id": 999 });
        assert_eq!(extract_subject_from_json(&v), Some("12345".to_owned()));
    }

    #[test]
    fn extract_stringifies_numeric_id() {
        // Notion / Cloudflare return numeric ids.
        let v = json!({ "id": 42 });
        assert_eq!(extract_subject_from_json(&v), Some("42".to_owned()));
    }

    #[test]
    fn extract_falls_back_to_email_when_no_id_fields() {
        let v = json!({ "email": "user@example.com" });
        assert_eq!(
            extract_subject_from_json(&v),
            Some("user@example.com".to_owned())
        );
    }

    #[test]
    fn extract_descends_into_user_wrapper() {
        // Slack-style: `{ "user": { ... } }`.
        let v = json!({ "user": { "id": "U123", "name": "alice" } });
        // `name` wins over `id` per SUBJECT_KEYS order.
        assert_eq!(extract_subject_from_json(&v), Some("alice".to_owned()));
    }

    #[test]
    fn extract_descends_into_data_wrapper() {
        // JSON:API-style.
        let v = json!({ "data": { "sub": "abc-123" } });
        assert_eq!(extract_subject_from_json(&v), Some("abc-123".to_owned()));
    }

    #[test]
    fn extract_descends_into_results_array() {
        // `results: [first]` — picks first element of array.
        let v = json!({ "results": [{ "login": "octocat" }, { "login": "other" }] });
        assert_eq!(extract_subject_from_json(&v), Some("octocat".to_owned()));
    }

    #[test]
    fn extract_does_not_recurse_two_levels() {
        // Org-level identifier under a wrapper should NOT be picked.
        // `team.org.id` is too deep — we only descend ONE level.
        let v = json!({ "team": { "org": { "id": "should-not-pick" } } });
        assert_eq!(extract_subject_from_json(&v), None);
    }

    #[test]
    fn extract_skips_empty_strings_and_null() {
        let v = json!({
            "preferred_username": "",
            "sub": null,
            "login": "real-id",
        });
        assert_eq!(extract_subject_from_json(&v), Some("real-id".to_owned()));
    }

    #[test]
    fn extract_returns_none_for_objects_without_known_keys() {
        let v = json!({ "totally": "unrelated", "field": 1 });
        assert_eq!(extract_subject_from_json(&v), None);
    }

    #[test]
    fn extract_returns_none_for_non_objects() {
        assert_eq!(extract_subject_from_json(&json!("string")), None);
        assert_eq!(extract_subject_from_json(&json!(42)), None);
        assert_eq!(extract_subject_from_json(&json!(null)), None);
        assert_eq!(extract_subject_from_json(&json!([])), None);
    }

    /// Helper: build a 3-segment JWT-shaped string with the given
    /// payload object (header + signature are placeholders since
    /// subject_from_jwt doesn't verify).
    fn make_unsigned_jwt(payload: serde_json::Value) -> String {
        use data_encoding::BASE64URL_NOPAD;
        let header = BASE64URL_NOPAD.encode(b"{\"alg\":\"none\"}");
        let body = BASE64URL_NOPAD.encode(payload.to_string().as_bytes());
        let sig = BASE64URL_NOPAD.encode(b"");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn jwt_subject_extracts_sub() {
        let token = make_unsigned_jwt(json!({ "sub": "user-123" }));
        assert_eq!(subject_from_jwt(&token), Some("user-123".to_owned()));
    }

    #[test]
    fn jwt_opaque_token_yields_none() {
        // Atlassian-style opaque token — not a JWT.
        assert_eq!(subject_from_jwt("not.a.jwt.extra"), None);
        assert_eq!(subject_from_jwt("opaque-token-no-dots"), None);
        assert_eq!(subject_from_jwt(""), None);
    }

    #[test]
    fn jwt_with_padded_base64_falls_through_to_padded_decoder() {
        // Some upstreams include `=` padding even though RFC 7515
        // forbids it. The fallback to BASE64URL (padded) handles
        // these — without it, RFC-violating tokens would fail to
        // decode and we'd silently leave upstream_subject NULL for
        // a class of providers.
        use data_encoding::BASE64URL;
        let payload =
            BASE64URL.encode(serde_json::json!({"sub": "user-xy"}).to_string().as_bytes());
        let header = BASE64URL.encode(b"{\"alg\":\"none\"}");
        let sig = BASE64URL.encode(b"");
        let token = format!("{header}.{payload}.{sig}");
        assert_eq!(subject_from_jwt(&token), Some("user-xy".to_owned()));
    }

    #[test]
    fn jwt_malformed_payload_yields_none() {
        // Three segments but middle isn't valid base64.
        assert_eq!(subject_from_jwt("a.!@#$.c"), None);
    }
}
