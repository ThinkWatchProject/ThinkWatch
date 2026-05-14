//! `JsonSecret` — at-rest representation of a sensitive string embedded
//! inside a JSONB column.
//!
//! Provider `config_json` carries multiple header values plus
//! `aws_secret_access_key`; each of those individual values is wrapped
//! as `{"$enc": "<hex>"}` in production rows, or left as a bare
//! plaintext string on legacy dev DBs that pre-date the at-rest
//! encryption migration. Centralising the wire shape here means the
//! next at-rest field added to a JSONB column reuses the same envelope
//! instead of inventing its own; tests can ask `JsonSecret::is_encrypted`
//! instead of reaching into `value.get("$enc")` directly.
//!
//! This module covers the *JSON-nested* case only. Column-level
//! ciphertexts (mcp_oauth client secret, totp_secret, etc.) already use
//! [`crypto::encrypt`] / [`crypto::decrypt`] against a dedicated
//! `BYTEA`/`String` column; they don't carry a JSON wrapper, so they
//! don't go through this type.

use crate::crypto;
use crate::errors::AppError;

/// Stored representation of a JSON-nested secret value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonSecret {
    /// Production shape — `{"$enc": "<hex AES-256-GCM envelope>"}`.
    Encrypted { hex: String },
    /// Legacy plaintext row predating the encryption migration. Loaded
    /// transparently; the caller surfaces a `tracing::warn!` so admins
    /// re-save to upgrade.
    LegacyPlain(String),
    /// Missing key, null, empty string, or any other shape we treat as
    /// "no value supplied". The loader fans this back into the empty
    /// string at the consumer boundary.
    Empty,
}

/// JSON marker key — only exported because the backfill task needs to
/// recognise already-wrapped rows without a full round-trip decrypt.
/// Production read paths should call [`JsonSecret::from_json`] instead.
pub const ENC_MARKER: &str = "$enc";

impl JsonSecret {
    /// Recognise the three valid on-disk shapes.
    pub fn from_json(value: &serde_json::Value) -> Self {
        if let Some(obj) = value.as_object()
            && let Some(hex_str) = obj.get(ENC_MARKER).and_then(|v| v.as_str())
        {
            return JsonSecret::Encrypted {
                hex: hex_str.to_string(),
            };
        }
        match value.as_str() {
            Some("") | None => JsonSecret::Empty,
            Some(s) => JsonSecret::LegacyPlain(s.to_string()),
        }
    }

    /// Encrypt `plaintext` and produce a value suitable for INSERT.
    /// Empty input yields [`JsonSecret::Empty`] — burning AES on `""`
    /// is wasteful and the loader treats missing/empty identically.
    pub fn encrypt(plaintext: &str, encryption_key: &str) -> Result<Self, AppError> {
        if plaintext.is_empty() {
            return Ok(JsonSecret::Empty);
        }
        let key = crypto::parse_encryption_key(encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        let bytes = crypto::encrypt(plaintext.as_bytes(), &key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Secret encrypt failed: {e}")))?;
        Ok(JsonSecret::Encrypted {
            hex: hex::encode(bytes),
        })
    }

    /// Resolve to plaintext + a `was_encrypted` flag. Empty values
    /// resolve to `("", false)`; legacy plaintext resolves to the
    /// stored bytes with `was_encrypted=false` (caller can surface a
    /// warn line to prompt re-save).
    pub fn decrypt(&self, encryption_key: &str) -> Result<(String, bool), AppError> {
        match self {
            JsonSecret::Encrypted { hex } => {
                let bytes = hex::decode(hex).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("Secret hex decode failed: {e}"))
                })?;
                let key = crypto::parse_encryption_key(encryption_key).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}"))
                })?;
                let plain = crypto::decrypt(&bytes, &key).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("Secret decrypt failed: {e}"))
                })?;
                let s = String::from_utf8(plain).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("Secret is not valid UTF-8: {e}"))
                })?;
                Ok((s, true))
            }
            JsonSecret::LegacyPlain(s) => Ok((s.clone(), false)),
            JsonSecret::Empty => Ok((String::new(), false)),
        }
    }

    /// Render to the JSON shape that goes into the DB. Empty stays as
    /// `""` rather than `null` so downstream readers that expect a
    /// string don't choke on a type change.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            JsonSecret::Encrypted { hex } => serde_json::json!({ ENC_MARKER: hex }),
            JsonSecret::LegacyPlain(s) => serde_json::Value::String(s.clone()),
            JsonSecret::Empty => serde_json::Value::String(String::new()),
        }
    }

    /// Cheap check used by tests + the backfill: was this value
    /// stored with the encryption envelope, or is it still legacy
    /// plaintext that needs re-wrapping?
    pub fn is_encrypted(&self) -> bool {
        matches!(self, JsonSecret::Encrypted { .. })
    }

    /// Convenience: classify a raw JSON value without holding the
    /// intermediate `JsonSecret`. Used in backfill loops where the
    /// caller only needs the boolean.
    pub fn json_is_encrypted(value: &serde_json::Value) -> bool {
        value
            .as_object()
            .is_some_and(|o| o.contains_key(ENC_MARKER))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key_hex() -> String {
        // 32 zero bytes — fine for unit tests.
        hex::encode([0u8; 32])
    }

    #[test]
    fn round_trip_encrypt_decrypt() {
        let key = test_key_hex();
        let enc = JsonSecret::encrypt("sk-secret", &key).unwrap();
        assert!(enc.is_encrypted());
        let (plain, was_enc) = enc.decrypt(&key).unwrap();
        assert_eq!(plain, "sk-secret");
        assert!(was_enc);
    }

    #[test]
    fn empty_is_not_encrypted() {
        let key = test_key_hex();
        let enc = JsonSecret::encrypt("", &key).unwrap();
        assert_eq!(enc, JsonSecret::Empty);
        assert!(!enc.is_encrypted());
        assert_eq!(enc.to_json(), serde_json::Value::String(String::new()));
    }

    #[test]
    fn from_json_recognises_all_three_shapes() {
        let key = test_key_hex();
        let enc = JsonSecret::encrypt("hello", &key).unwrap();
        let wire = enc.to_json();
        assert_eq!(JsonSecret::from_json(&wire), enc);

        let legacy = serde_json::Value::String("plain".into());
        assert_eq!(
            JsonSecret::from_json(&legacy),
            JsonSecret::LegacyPlain("plain".into())
        );

        let empty_str = serde_json::Value::String(String::new());
        assert_eq!(JsonSecret::from_json(&empty_str), JsonSecret::Empty);
        assert_eq!(
            JsonSecret::from_json(&serde_json::Value::Null),
            JsonSecret::Empty
        );
    }

    #[test]
    fn legacy_plain_decrypts_to_itself_with_was_encrypted_false() {
        let v = JsonSecret::LegacyPlain("old-key".into());
        let (plain, was_enc) = v.decrypt(&test_key_hex()).unwrap();
        assert_eq!(plain, "old-key");
        assert!(!was_enc);
    }

    #[test]
    fn json_is_encrypted_detects_envelope() {
        let enc = JsonSecret::encrypt("x", &test_key_hex()).unwrap().to_json();
        assert!(JsonSecret::json_is_encrypted(&enc));
        assert!(!JsonSecret::json_is_encrypted(&serde_json::Value::String(
            "x".into()
        )));
        assert!(!JsonSecret::json_is_encrypted(&serde_json::Value::Null));
    }
}
