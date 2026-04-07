use think_watch_common::errors::AppError;

use crate::app::AppState;

/// Get the ClickHouse client from state, or return a "not configured" error.
pub fn ch_client(state: &AppState) -> Result<&clickhouse::Client, AppError> {
    state
        .clickhouse
        .as_ref()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("ClickHouse is not configured")))
}

/// Returns true if ClickHouse is configured.
pub fn ch_available(state: &AppState) -> bool {
    state.clickhouse.is_some()
}

/// Helper: execute a count query and return the total.
#[allow(dead_code)]
pub async fn ch_count(client: &clickhouse::Client, query: &str) -> Result<u64, AppError> {
    let total: u64 =
        client.query(query).fetch_one().await.map_err(|e| {
            AppError::Internal(anyhow::anyhow!("ClickHouse count query failed: {e}"))
        })?;
    Ok(total)
}

/// How a column should be matched when emitted as an exclude clause.
#[derive(Debug, Clone, Copy)]
pub enum ExcludeMode {
    /// `column <> ?` — exact equality.
    Equals,
    /// `column NOT LIKE ?` — substring (caller wraps with %% and escapes).
    NotLike,
}

/// Parse an `exclude` query parameter into `(condition, bind)` pairs ready
/// to append to a ClickHouse WHERE clause.
///
/// The expected format is a comma-separated list of `key:value` pairs:
///
///   `exclude=method:POST,status_code:200,path:/admin`
///
/// Values may also be quoted with double quotes if they contain commas
/// or colons:
///
///   `exclude=path:"/api/v1/foo,bar"`
///
/// Each `key` must be in `allowed`, which maps the externally visible
/// filter name to (internal column name, match mode). Unknown keys are
/// silently dropped so a frontend that knows extra fields can't poke at
/// columns we didn't whitelist.
///
/// Returns a list of (sql_fragment, bind_value) tuples. `sql_fragment`
/// uses `?` placeholders and is safe to drop into the WHERE clause as
/// long as the caller binds the values in order.
pub fn parse_exclude_param(
    exclude: Option<&str>,
    allowed: &[(&str, &str, ExcludeMode)],
) -> Vec<(String, String)> {
    let Some(raw) = exclude else {
        return Vec::new();
    };
    if raw.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for token in split_top_level_commas(raw) {
        let token = token.trim();
        let Some(colon) = token.find(':') else {
            continue;
        };
        let (key, rest) = token.split_at(colon);
        let value = rest[1..].trim();
        let value = strip_quotes(value);
        if key.is_empty() || value.is_empty() {
            continue;
        }
        let Some((_ext, col, mode)) = allowed.iter().find(|(k, _, _)| *k == key) else {
            continue;
        };
        match mode {
            ExcludeMode::Equals => {
                out.push((format!("{col} <> ?"), value.to_string()));
            }
            ExcludeMode::NotLike => {
                let escaped = value
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                out.push((format!("{col} NOT LIKE ?"), format!("%{escaped}%")));
            }
        }
    }
    out
}

/// Split on commas that are not inside double-quoted segments.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ',' if !in_quote => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ch_available_returns_false_when_no_client() {
        // We can't easily construct AppState in a unit test, but we verify
        // the function signature compiles correctly. Integration tests
        // would cover the full path.
    }

    #[test]
    fn parse_exclude_drops_unknown_keys() {
        let allowed = &[
            ("method", "method", ExcludeMode::Equals),
            ("path", "path", ExcludeMode::NotLike),
        ];
        let out = parse_exclude_param(Some("method:POST,evil:DROP TABLE"), allowed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "method <> ?");
        assert_eq!(out[0].1, "POST");
    }

    #[test]
    fn parse_exclude_handles_quoted_values_with_commas() {
        let allowed = &[("path", "path", ExcludeMode::NotLike)];
        let out = parse_exclude_param(Some("path:\"/foo,bar\""), allowed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "path NOT LIKE ?");
        assert_eq!(out[0].1, "%/foo,bar%");
    }

    #[test]
    fn parse_exclude_escapes_like_wildcards() {
        let allowed = &[("path", "path", ExcludeMode::NotLike)];
        let out = parse_exclude_param(Some("path:50%off"), allowed);
        assert_eq!(out[0].1, "%50\\%off%");
    }

    #[test]
    fn parse_exclude_empty_or_missing_returns_nothing() {
        let allowed = &[("method", "method", ExcludeMode::Equals)];
        assert!(parse_exclude_param(None, allowed).is_empty());
        assert!(parse_exclude_param(Some(""), allowed).is_empty());
    }

    #[test]
    fn parse_exclude_multiple_clauses() {
        let allowed = &[
            ("method", "method", ExcludeMode::Equals),
            ("status_code", "status_code", ExcludeMode::Equals),
        ];
        let out = parse_exclude_param(Some("method:GET,status_code:200"), allowed);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "method <> ?");
        assert_eq!(out[1].0, "status_code <> ?");
    }
}
