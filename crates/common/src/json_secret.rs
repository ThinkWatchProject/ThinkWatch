//! `JsonSecret` — at-rest representation of a sensitive string embedded
//! inside a JSONB column.
//!
//! Provider `config_json` carries multiple header values plus
//! `aws_secret_access_key`; each of those individual values is wrapped
//! as `{"$enc": "<hex>"}` in production rows. Centralising the wire
//! shape here means the next at-rest field added to a JSONB column
//! reuses the same envelope instead of inventing its own; tests can
//! ask `JsonSecret::is_encrypted` instead of reaching into
//! `value.get("$enc")` directly.
//!
//! This module covers the *JSON-nested* case only. Column-level
//! ciphertexts (mcp_oauth client secret, totp_secret, etc.) already use
//! [`crypto::encrypt`] / [`crypto::decrypt`] against a dedicated
//! `BYTEA`/`String` column; they don't carry a JSON wrapper, so they
//! don't go through this type.

use crate::crypto;
use crate::errors::AppError;

/// A secret that will not decrypt is always `Internal`: the caller supplied
/// a well-formed request, and the failure is either a wrong key or a
/// corrupted ciphertext — both of which are ours to fix, not theirs.
/// Mapping it to `BadRequest` would tell a user to change something they
/// never controlled.
fn internal(msg: impl std::fmt::Display) -> AppError {
    AppError::Internal(anyhow::anyhow!("{msg}"))
}

/// Stored representation of a JSON-nested secret value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonSecret {
    /// `{"$enc": "<hex AES-256-GCM envelope>"}`.
    Encrypted { hex: String },
    /// Missing key, null, empty string, or any other shape we treat as
    /// "no value supplied". The loader fans this back into the empty
    /// string at the consumer boundary.
    Empty,
}

/// JSON marker key — only exported because tests need to recognise
/// already-wrapped rows without a full round-trip decrypt. Production
/// read paths should call [`JsonSecret::from_json`] instead.
pub const ENC_MARKER: &str = "$enc";

impl JsonSecret {
    /// Recognise the valid on-disk shapes. Returns `Err` for a bare
    /// non-empty string — that shape is never written by any producer
    /// in this codebase, so encountering one at read time signals a
    /// corrupted row or a hand-edited DB. Surfacing the error stops
    /// us from silently swallowing the cipher and serving a "no
    /// credentials" downstream error that's much harder to trace.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, AppError> {
        if let Some(obj) = value.as_object()
            && let Some(hex_str) = obj.get(ENC_MARKER).and_then(|v| v.as_str())
        {
            return Ok(JsonSecret::Encrypted {
                hex: hex_str.to_string(),
            });
        }
        match value.as_str() {
            Some("") | None => Ok(JsonSecret::Empty),
            Some(_) => Err(internal(format!(
                "config_json secret is a bare string; expected `{{\"{ENC_MARKER}\":...}}` or null",
            ))),
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
            .map_err(|e| internal(format!("Invalid encryption key: {e}")))?;
        let bytes = crypto::encrypt(plaintext.as_bytes(), &key)
            .map_err(|e| internal(format!("Secret encrypt failed: {e}")))?;
        Ok(JsonSecret::Encrypted {
            hex: hex::encode(bytes),
        })
    }

    /// Resolve to plaintext. `Empty` resolves to `""`; `Encrypted`
    /// decrypts via the workspace's at-rest key.
    pub fn decrypt(&self, encryption_key: &str) -> Result<String, AppError> {
        match self {
            JsonSecret::Encrypted { hex } => {
                let bytes = hex::decode(hex)
                    .map_err(|e| internal(format!("Secret hex decode failed: {e}")))?;
                let key = crypto::parse_encryption_key(encryption_key)
                    .map_err(|e| internal(format!("Invalid encryption key: {e}")))?;
                let plain = crypto::decrypt(&bytes, &key)
                    .map_err(|e| internal(format!("Secret decrypt failed: {e}")))?;
                String::from_utf8(plain)
                    .map_err(|e| internal(format!("Secret is not valid UTF-8: {e}")))
            }
            JsonSecret::Empty => Ok(String::new()),
        }
    }

    /// Render to the JSON shape that goes into the DB. Empty stays as
    /// `""` rather than `null` so downstream readers that expect a
    /// string don't choke on a type change.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            JsonSecret::Encrypted { hex } => serde_json::json!({ ENC_MARKER: hex }),
            JsonSecret::Empty => serde_json::Value::String(String::new()),
        }
    }

    /// Cheap check used by tests: was this value stored with the
    /// encryption envelope, or is it `Empty`?
    pub fn is_encrypted(&self) -> bool {
        matches!(self, JsonSecret::Encrypted { .. })
    }

    /// Convenience: classify a raw JSON value without holding the
    /// intermediate `JsonSecret`. The write path uses this to skip
    /// double-encrypting an `{"$enc":...}` shape that round-tripped
    /// from a GET response.
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
        let plain = enc.decrypt(&key).unwrap();
        assert_eq!(plain, "sk-secret");
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
    fn from_json_recognises_enc_envelope_and_empty() {
        let key = test_key_hex();
        let enc = JsonSecret::encrypt("hello", &key).unwrap();
        let wire = enc.to_json();
        assert_eq!(JsonSecret::from_json(&wire).unwrap(), enc);

        let empty_str = serde_json::Value::String(String::new());
        assert_eq!(
            JsonSecret::from_json(&empty_str).unwrap(),
            JsonSecret::Empty
        );
        assert_eq!(
            JsonSecret::from_json(&serde_json::Value::Null).unwrap(),
            JsonSecret::Empty
        );
    }

    #[test]
    fn from_json_rejects_bare_non_empty_string() {
        // No producer in this codebase ever writes a bare string into
        // a secret slot — encountering one at read time means the row
        // is corrupted or hand-edited. The reader returns Err so the
        // caller can surface the misconfig instead of silently
        // treating it as Empty and falling through to "no credentials".
        let bare = serde_json::Value::String("hand-typed-secret".into());
        assert!(JsonSecret::from_json(&bare).is_err());
    }
}
