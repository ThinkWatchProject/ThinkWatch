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
