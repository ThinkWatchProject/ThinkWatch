//! Saved secrets: the credentials this gateway holds for its providers,
//! which outbound redaction recognises by value (the built-in rule
//! `thinkwatch-saved-secret`).
//!
//! Many providers' keys have no prefix to recognise them by — a 32-digit
//! hex string, a UUID — yet the gateway knows each one exactly, because it
//! sends them. A caller pasting one into a prompt is caught whatever its
//! shape. The security log names the provider whose key it was, never any
//! part of the value.
//!
//! The list is collected from the providers whenever they are loaded
//! (startup, every provider CRUD, and the cross-instance reload), and the
//! guards are recompiled with it; see `think_watch_server::app`.

use std::fmt;

use tw_guard::redact::rules::RuleSet;

/// Values shorter than this are not matched: a short value is too likely
/// to occur by chance. The same floor thinkwatch-core applies.
pub const MIN_LEN: usize = 12;

/// Header names whose value is a credential: `authorization`, `x-api-key`,
/// `api-key`, `x-goog-api-key`, and any custom header that says it carries
/// a key, token, secret or password. Other headers a provider sends
/// (`anthropic-version`, `content-type`) are configuration, not secrets,
/// and matching their values would flag every request that mentions them.
fn credential_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["auth", "key", "token", "secret", "password"]
        .iter()
        .any(|w| name.contains(w))
}

/// The credential in a header value: the token after an auth scheme
/// (`Bearer sk-…` → `sk-…`), or the whole value.
fn credential_value(value: &str) -> &str {
    let value = value.trim();
    match value.split_once(' ') {
        Some((scheme, token))
            if !scheme.is_empty()
                && scheme.chars().all(|c| c.is_ascii_alphabetic())
                && !token.trim().is_empty()
                && !token.trim().contains(' ') =>
        {
            token.trim()
        }
        _ => value,
    }
}

/// One provider credential.
#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamSecret {
    /// The provider's name, as the security log names the source.
    pub provider: String,
    pub value: String,
    /// The host of the provider's base URL: the credential's own
    /// destination, which dangerous commands' "credential sent to an
    /// unknown host" does not count as sending it out. `None` when the
    /// provider has no base URL to read one from.
    pub host: Option<String>,
}

impl fmt::Debug for UpstreamSecret {
    /// Never the value: this ends up in logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamSecret")
            .field("provider", &self.provider)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// What a provider holds, decrypted, for [`SavedSecrets::of_provider`].
#[derive(Debug, Clone, Copy)]
pub struct ProviderCredentials<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    /// The headers sent upstream, `(name, value)`.
    pub headers: &'a [(String, String)],
    /// A Bedrock provider's stored secret access key.
    pub aws_secret_access_key: Option<&'a str>,
}

/// Every provider credential, each value once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedSecrets {
    list: Vec<UpstreamSecret>,
}

impl SavedSecrets {
    /// The credentials of these providers. A value shorter than
    /// [`MIN_LEN`] is left out; a value two providers share is kept once,
    /// under the first.
    pub fn collect<'a>(providers: impl IntoIterator<Item = ProviderCredentials<'a>>) -> Self {
        let mut list: Vec<UpstreamSecret> = Vec::new();
        for p in providers {
            let host = url::Url::parse(p.base_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
            let values = p
                .headers
                .iter()
                .filter(|(name, _)| credential_header(name))
                .map(|(_, value)| credential_value(value))
                .chain(p.aws_secret_access_key.map(str::trim));
            for value in values {
                if value.chars().count() < MIN_LEN || list.iter().any(|s| s.value == value) {
                    continue;
                }
                list.push(UpstreamSecret {
                    provider: p.name.to_string(),
                    value: value.to_string(),
                    host: host.clone(),
                });
            }
        }
        Self { list }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &UpstreamSecret> {
        self.list.iter()
    }

    /// The providers whose credential appears in `text`, as written or
    /// JSON-escaped, each name once, in list order. For the security log
    /// of a request the saved-secret rule matched in: which credential it
    /// was, named by its provider.
    pub fn providers_in(&self, text: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for s in &self.list {
            if out.contains(&s.provider.as_str()) {
                continue;
            }
            let escaped = serde_json::to_string(&s.value).unwrap_or_default();
            let escaped = escaped
                .strip_prefix('"')
                .and_then(|e| e.strip_suffix('"'))
                .unwrap_or_default();
            if text.contains(s.value.as_str()) || (escaped != s.value && text.contains(escaped)) {
                out.push(&s.provider);
            }
        }
        out
    }
}

/// `rules` with the saved secrets added, recognised by value. **The one
/// place the saved secrets reach the rule set.**
pub fn with_saved(rules: RuleSet, saved: &SavedSecrets) -> RuleSet {
    if saved.is_empty() {
        return rules;
    }
    // TODO(thinkwatch-core `RuleSet::with_saved`, branch feat/redact-secrets):
    // hand the list over once core has it, then re-pin core to the release:
    //
    //     rules.with_saved(
    //         saved
    //             .iter()
    //             .map(|s| SavedSecret {
    //                 value: s.value.clone(),
    //                 source: SavedSource { kind: SavedSourceKind::UpstreamKey, name: s.provider.clone() },
    //                 hosts: s.host.iter().cloned().collect(),
    //             })
    //             .collect(),
    //     )
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    fn provider<'a>(
        name: &'a str,
        base_url: &'a str,
        headers: &'a [(String, String)],
    ) -> ProviderCredentials<'a> {
        ProviderCredentials {
            name,
            base_url,
            headers,
            aws_secret_access_key: None,
        }
    }

    #[test]
    fn credential_headers_are_collected_and_configuration_headers_are_not() {
        let headers = [
            h("x-api-key", "test-anthropic-key-0123456789"),
            h("anthropic-version", "2023-06-01-long-enough"),
            h("Content-Type", "application/json"),
        ];
        let s =
            SavedSecrets::collect([provider("claude", "https://API.anthropic.com/v1", &headers)]);
        let got: Vec<(&str, &str, Option<&str>)> = s
            .iter()
            .map(|s| (s.provider.as_str(), s.value.as_str(), s.host.as_deref()))
            .collect();
        assert_eq!(
            got,
            [(
                "claude",
                "test-anthropic-key-0123456789",
                Some("api.anthropic.com")
            )]
        );
    }

    #[test]
    fn the_auth_scheme_is_not_part_of_the_value() {
        let headers = [
            h("Authorization", "Bearer test-deepseek-key-abcdef"),
            h("X-Custom-Token", "  test-custom-token-123456  "),
            h("api-key", "test-azure-key-0123456789"),
            h("x-goog-api-key", "test-google-key-0123456789"),
        ];
        let s = SavedSecrets::collect([provider("deepseek", "https://api.deepseek.com", &headers)]);
        let values: Vec<&str> = s.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "test-deepseek-key-abcdef",
                "test-custom-token-123456",
                "test-azure-key-0123456789",
                "test-google-key-0123456789",
            ]
        );
    }

    #[test]
    fn short_values_empty_values_and_duplicates_are_left_out() {
        let a = [
            h("Authorization", "Bearer "),
            h("x-api-key", "short-key"),
            h("x-api-key", "test-shared-key-0123456789"),
        ];
        let b = [h("Authorization", "Bearer test-shared-key-0123456789")];
        let s = SavedSecrets::collect([
            provider("first", "https://one.example.com", &a),
            provider("second", "https://two.example.com", &b),
        ]);
        assert_eq!(s.len(), 1);
        let only = s.iter().next().unwrap();
        assert_eq!(only.provider, "first", "kept under the first provider");
    }

    #[test]
    fn a_bedrock_secret_access_key_is_collected() {
        let s = SavedSecrets::collect([ProviderCredentials {
            name: "bedrock",
            base_url: "",
            headers: &[],
            aws_secret_access_key: Some("test/secret+access/key0123456789"),
        }]);
        let only = s.iter().next().unwrap();
        assert_eq!(only.value, "test/secret+access/key0123456789");
        assert_eq!(only.host, None, "no base URL, no host");
    }

    #[test]
    fn debug_never_shows_the_value() {
        let headers = [h("x-api-key", "test-hidden-key-0123456789")];
        let s = SavedSecrets::collect([provider("p", "https://p.example.com", &headers)]);
        let shown = format!("{s:?}");
        assert!(!shown.contains("test-hidden-key"), "{shown}");
        assert!(shown.contains("p.example.com"), "{shown}");
    }

    #[test]
    fn providers_in_names_whose_credential_a_text_carries() {
        let a = [h("x-api-key", "test-alpha-key-0123456789")];
        let b = [h("x-api-key", "test\"beta\\key-0123456789")];
        let c = [h("x-api-key", "test-gamma-key-0123456789")];
        let s = SavedSecrets::collect([
            provider("alpha", "https://a.example.com", &a),
            provider("beta", "https://b.example.com", &b),
            provider("gamma", "https://c.example.com", &c),
        ]);
        let body = r#"{"content":"use test-alpha-key-0123456789 and test\"beta\\key-0123456789"}"#;
        assert_eq!(s.providers_in(body), ["alpha", "beta"]);
        assert!(s.providers_in("nothing here").is_empty());
    }

    #[test]
    fn no_saved_secrets_leaves_the_rules_as_they_are() {
        let rules = with_saved(RuleSet::only(&["jwt"]), &SavedSecrets::default());
        assert!(rules.is_on("jwt") && !rules.is_on("anthropic-api-key"));
    }
}
