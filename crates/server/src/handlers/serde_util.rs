use serde::{Deserialize, Deserializer};

/// Wrap a `T`-deserializer so that `Option<T>` distinguishes "field
/// absent" from "field is null". Standard serde collapses both to
/// `None`, which is wrong for PATCH semantics where `null` should
/// mean "clear this field" and an absent field should mean "leave
/// it unchanged".
///
/// Combined with `Option<Option<T>>` + `#[serde(default)]` the three
/// states encode cleanly:
///   - field absent       → `None`               ("don't touch")
///   - JSON `null`        → `Some(None)`         ("clear")
///   - JSON value         → `Some(Some(v))`      ("replace")
///
/// SQL side wants the matching `(set, value)` split:
///
/// ```ignore
/// let (set, value) = match &req.field {
///     None => (false, None),
///     Some(inner) => (true, inner.as_deref()),
/// };
/// // …WHERE bind ($set, $value)…
/// // SET col = CASE WHEN $set THEN $value ELSE col END
/// ```
pub fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::deserialize_some;
    use serde::Deserialize;

    // Concrete usage: a PATCH request body where each field encodes
    // tri-state via `Option<Option<String>>`. Each test asserts a
    // different cell of the absent/null/value matrix.
    #[derive(Debug, Default, Deserialize, PartialEq)]
    struct PatchReq {
        #[serde(default, deserialize_with = "deserialize_some")]
        name: Option<Option<String>>,
    }

    #[test]
    fn absent_field_deserializes_to_outer_none() {
        // "don't touch" semantics — caller didn't include the field.
        let parsed: PatchReq = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn null_field_deserializes_to_some_none() {
        // "clear this field" — explicit JSON null.
        let parsed: PatchReq = serde_json::from_str(r#"{"name": null}"#).unwrap();
        assert_eq!(parsed.name, Some(None));
    }

    #[test]
    fn value_field_deserializes_to_some_some() {
        // "replace with this value".
        let parsed: PatchReq = serde_json::from_str(r#"{"name": "alice"}"#).unwrap();
        assert_eq!(parsed.name, Some(Some("alice".into())));
    }

    #[test]
    fn empty_string_value_still_some_some_not_some_none() {
        // Empty string is a *value*, not "clear". The handler may
        // decide to treat it as clear at the validation layer, but the
        // wire-level deserialization MUST distinguish the two.
        let parsed: PatchReq = serde_json::from_str(r#"{"name": ""}"#).unwrap();
        assert_eq!(parsed.name, Some(Some(String::new())));
    }

    #[test]
    fn wrong_type_surfaces_as_error_not_silent_none() {
        // A type mismatch must be a deserialize error, not silently
        // dropped to None — otherwise the API would accept invalid
        // payloads and ignore them.
        let result: Result<PatchReq, _> = serde_json::from_str(r#"{"name": 42}"#);
        assert!(result.is_err());
    }
}
