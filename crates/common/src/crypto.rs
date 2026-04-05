use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};

/// Encrypt data using AES-256-GCM.
/// Returns: 12-byte nonce || ciphertext || 16-byte tag (all concatenated).
pub fn encrypt(plaintext: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow::anyhow!("Invalid key: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    // Prepend nonce to ciphertext
    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt data encrypted with `encrypt()`.
/// Input: 12-byte nonce || ciphertext || 16-byte tag.
pub fn decrypt(encrypted: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    if encrypted.len() < 12 {
        return Err(anyhow::anyhow!("Ciphertext too short"));
    }

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow::anyhow!("Invalid key: {e}"))?;

    let nonce = Nonce::from_slice(&encrypted[..12]);
    let ciphertext = &encrypted[12..];

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {e}"))
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
}
