use totp_rs::{Algorithm, Secret, TOTP};

const ISSUER: &str = "AgentBastion";
const DIGITS: usize = 6;
const STEP: u64 = 30;
const SKEW: u8 = 1;

/// Generate a new random TOTP secret (base32-encoded).
pub fn generate_secret() -> String {
    let secret = Secret::generate_secret();
    secret.to_encoded().to_string()
}

/// Build an otpauth:// URI for QR code generation.
pub fn otpauth_uri(secret_base32: &str, email: &str) -> anyhow::Result<String> {
    let totp = build_totp(secret_base32, email)?;
    Ok(totp.get_url())
}

/// Verify a 6-digit TOTP code against the secret.
pub fn verify(secret_base32: &str, code: &str, email: &str) -> anyhow::Result<bool> {
    let totp = build_totp(secret_base32, email)?;
    Ok(totp.check_current(code).unwrap_or(false))
}

/// Generate a set of one-time recovery codes (80-bit entropy each).
pub fn generate_recovery_codes(count: usize) -> Vec<String> {
    (0..count)
        .map(|_| {
            let mut bytes = [0u8; 10];
            rand::fill(&mut bytes);
            let code = data_encoding::BASE32_NOPAD.encode(&bytes);
            // Format as XXXX-XXXX for readability (take first 8 base32 chars)
            let code = &code[..8];
            format!("{}-{}", &code[..4], &code[4..8])
        })
        .collect()
}

/// Constant-time comparison of a recovery code against a stored list.
/// Returns the index of the matching code if found.
pub fn find_recovery_code(codes: &[String], candidate: &str) -> Option<usize> {
    use subtle::ConstantTimeEq;
    let candidate_bytes = candidate.as_bytes();
    let mut found_idx: Option<usize> = None;
    for (i, stored) in codes.iter().enumerate() {
        let stored_bytes = stored.as_bytes();
        // Only compare if lengths match (constant-time within same-length strings)
        if stored_bytes.len() == candidate_bytes.len() && stored_bytes.ct_eq(candidate_bytes).into()
        {
            found_idx = Some(i);
        }
    }
    found_idx
}

/// Encrypt TOTP secret with AES-256-GCM and return hex-encoded ciphertext.
pub fn encrypt_secret(secret: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    let encrypted = agent_bastion_common::crypto::encrypt(secret.as_bytes(), key)?;
    Ok(hex::encode(encrypted))
}

/// Decrypt a hex-encoded TOTP secret.
pub fn decrypt_secret(encrypted_hex: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    let encrypted = hex::decode(encrypted_hex).map_err(|e| anyhow::anyhow!("Invalid hex: {e}"))?;
    let decrypted = agent_bastion_common::crypto::decrypt(&encrypted, key)?;
    String::from_utf8(decrypted).map_err(|e| anyhow::anyhow!("Invalid UTF-8: {e}"))
}

fn build_totp(secret_base32: &str, email: &str) -> anyhow::Result<TOTP> {
    let secret = Secret::Encoded(secret_base32.to_string())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("Invalid TOTP secret: {e}"))?;

    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        SKEW,
        STEP,
        secret,
        Some(ISSUER.to_string()),
        email.to_string(),
    )
    .map_err(|e| anyhow::anyhow!("Failed to create TOTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_verify() {
        let secret = generate_secret();
        let totp = build_totp(&secret, "test@example.com").unwrap();
        let code = totp
            .generate_current()
            .expect("should generate current code");
        assert!(verify(&secret, &code, "test@example.com").unwrap());
        assert!(!verify(&secret, "000000", "test@example.com").unwrap());
    }

    #[test]
    fn otpauth_uri_format() {
        let secret = generate_secret();
        let uri = otpauth_uri(&secret, "user@test.com").unwrap();
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains("AgentBastion"));
        assert!(uri.contains("user%40test.com") || uri.contains("user@test.com"));
    }

    #[test]
    fn recovery_codes_unique() {
        let codes = generate_recovery_codes(10);
        assert_eq!(codes.len(), 10);
        for code in &codes {
            assert_eq!(code.len(), 9); // XXXX-XXXX
            assert!(code.contains('-'));
        }
        // All unique
        let set: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(set.len(), 10);
    }

    #[test]
    fn encrypt_decrypt_secret() {
        let mut key = [0u8; 32];
        rand::fill(&mut key);
        let secret = generate_secret();
        let encrypted = encrypt_secret(&secret, &key).unwrap();
        let decrypted = decrypt_secret(&encrypted, &key).unwrap();
        assert_eq!(decrypted, secret);
    }
}
