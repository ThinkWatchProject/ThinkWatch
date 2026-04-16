use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};

/// Magic prefix for versioned ciphertexts. Every payload is parsed as
/// `MAGIC(4) || version(1) || nonce(12) || ciphertext + tag`.
const ENVELOPE_MAGIC: [u8; 4] = [0xfe, b'T', b'W', 0x01];

/// Current envelope version. Increment this when introducing a new
/// algorithm or KDF.
pub const CURRENT_KEY_VERSION: u8 = 1;

/// Encrypt data using AES-256-GCM, emitting a versioned envelope:
///
///   `[ENVELOPE_MAGIC (4)] [version (1)] [nonce (12)] [ciphertext + tag]`
pub fn encrypt(plaintext: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow::anyhow!("Invalid key: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    let mut result = Vec::with_capacity(4 + 1 + 12 + ciphertext.len());
    result.extend_from_slice(&ENVELOPE_MAGIC);
    result.push(CURRENT_KEY_VERSION);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt data produced by `encrypt`. Only accepts the versioned envelope
/// format: `[ENVELOPE_MAGIC (4)] [version (1)] [nonce (12)] [ciphertext + tag]`.
pub fn decrypt(encrypted: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    if encrypted.len() < 4 + 1 + 12 {
        return Err(anyhow::anyhow!("Ciphertext too short"));
    }
    if encrypted[..4] != ENVELOPE_MAGIC {
        return Err(anyhow::anyhow!(
            "Unrecognized ciphertext format (missing envelope magic)"
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow::anyhow!("Invalid key: {e}"))?;

    let version = encrypted[4];
    match version {
        1 => {
            let nonce = Nonce::from_slice(&encrypted[5..17]);
            let ciphertext = &encrypted[17..];
            cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| anyhow::anyhow!("Decryption failed: {e}"))
        }
        other => Err(anyhow::anyhow!(
            "Unknown ciphertext version: {other} (max supported: {CURRENT_KEY_VERSION})"
        )),
    }
}

/// Parse a 32-byte hex-encoded encryption key.
pub fn parse_encryption_key(hex_key: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(hex_key)?;
    if bytes.len() != 32 {
        return Err(anyhow::anyhow!(
            "Encryption key must be 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        rand::fill(&mut key);
        key
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plaintext = b"Hello, ThinkWatch!";
        let encrypted = encrypt(plaintext, &key).expect("encrypt should succeed");
        let decrypted = decrypt(&encrypted, &key).expect("decrypt should succeed");
        assert_eq!(decrypted, plaintext, "decrypted text must match original");
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let key1 = test_key();
        let mut key2 = test_key();
        // Ensure key2 differs from key1
        key2[0] ^= 0xFF;

        let encrypted = encrypt(b"secret data", &key1).expect("encrypt should succeed");
        let result = decrypt(&encrypted, &key2);
        assert!(result.is_err(), "decryption with wrong key must fail");
    }

    #[test]
    fn parse_encryption_key_valid() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let key = parse_encryption_key(hex_key).expect("should parse valid 64-hex-char key");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn parse_encryption_key_too_short() {
        let result = parse_encryption_key("abcdef");
        assert!(result.is_err(), "short key should be rejected");
    }

    #[test]
    fn parse_encryption_key_invalid_hex() {
        let result = parse_encryption_key(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        );
        assert!(result.is_err(), "non-hex chars should be rejected");
    }

    #[test]
    fn versioned_envelope_starts_with_magic() {
        let key = test_key();
        let encrypted = encrypt(b"hello", &key).unwrap();
        assert!(encrypted.starts_with(&ENVELOPE_MAGIC));
        assert_eq!(encrypted[4], CURRENT_KEY_VERSION);
    }

    #[test]
    fn unknown_version_rejected() {
        let key = test_key();
        // Manually build a ciphertext with an unknown version byte
        let mut bad = Vec::new();
        bad.extend_from_slice(&ENVELOPE_MAGIC);
        bad.push(99); // unknown version
        bad.extend_from_slice(&[0u8; 12]); // nonce
        bad.extend_from_slice(&[0u8; 16]); // tag-only ciphertext (will fail anyway)
        let result = decrypt(&bad, &key);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("Unknown ciphertext version"));
    }
}
