use totp_rs::{Algorithm, Builder, Secret, Totp};

const ISSUER: &str = "ThinkWatch";
const DIGITS: u8 = 6;
const STEP: u64 = 30;
const SKEW: u16 = 1;

/// Generate a new random TOTP secret (base32-encoded).
pub fn generate_secret() -> String {
    Secret::generate().to_base32()
}

/// Build an otpauth:// URI for QR code generation.
pub fn otpauth_uri(secret_base32: &str, email: &str) -> anyhow::Result<String> {
    let totp = build_totp(secret_base32, email)?;
    totp.to_url()
        .map_err(|e| anyhow::anyhow!("TOTP URI failed: {e}"))
}

/// Verify a 6-digit TOTP code against the secret.
pub fn verify(secret_base32: &str, code: &str, email: &str) -> anyhow::Result<bool> {
    let totp = build_totp(secret_base32, email)?;
    Ok(totp.check_current(code).is_some())
}

/// Compute the current 6-digit TOTP code for the given secret +
/// email pair. Used by integration tests that need to drive the
/// TOTP login flow end-to-end without typing into a real
/// authenticator app.
pub fn current_code(secret_base32: &str, email: &str) -> anyhow::Result<String> {
    let totp = build_totp(secret_base32, email)?;
    Ok(totp.generate_current().to_string())
}

/// Generate a set of one-time recovery codes (80-bit entropy each).
///
/// Codes are guaranteed unique within the returned set. The raw 40-bit
/// presentation space (8 base32 chars) makes birthday-paradox collisions
/// vanishingly rare for typical counts (~1 in 2^39 per pair), but the
/// codes live in a single AES-256-GCM-encrypted JSON blob in
/// `totp_recovery_codes` (same envelope as `totp_secret`) with no
/// DB-side uniqueness constraint — a duplicate inside the blob would
/// let one code be consumed twice. Dedup at generation so that's
/// impossible by construction.
pub fn generate_recovery_codes(count: usize) -> Vec<String> {
    let mut codes = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::with_capacity(count);
    while codes.len() < count {
        let mut bytes = [0u8; 10];
        rand::fill(&mut bytes);
        let encoded = data_encoding::BASE32_NOPAD.encode(&bytes);
        let code = format!("{}-{}", &encoded[..4], &encoded[4..8]);
        if seen.insert(code.clone()) {
            codes.push(code);
        }
    }
    codes
}

/// Constant-time comparison of a recovery code against a stored list.
/// Returns the index of the matching code if found.
///
/// All codes are padded/truncated to a fixed 16-byte buffer before comparison
/// so that the length check does not leak timing information.
pub fn find_recovery_code(codes: &[String], candidate: &str) -> Option<usize> {
    use subtle::ConstantTimeEq;

    const FIXED_LEN: usize = 16;

    fn pad_to_fixed(s: &[u8]) -> [u8; FIXED_LEN] {
        let mut buf = [0u8; FIXED_LEN];
        let copy_len = s.len().min(FIXED_LEN);
        buf[..copy_len].copy_from_slice(&s[..copy_len]);
        buf
    }

    let candidate_padded = pad_to_fixed(candidate.as_bytes());
    let candidate_len = candidate.len();
    let mut found_idx: Option<usize> = None;

    for (i, stored) in codes.iter().enumerate() {
        let stored_padded = pad_to_fixed(stored.as_bytes());
        // Both length and content are compared in constant time
        let len_match = (stored.len() as u8).ct_eq(&(candidate_len as u8));
        let content_match = stored_padded.ct_eq(&candidate_padded);
        if (len_match & content_match).into() {
            found_idx = Some(i);
        }
    }
    found_idx
}

/// Encrypt TOTP secret with AES-256-GCM and return hex-encoded ciphertext.
pub fn encrypt_secret(secret: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    let encrypted = think_watch_common::crypto::encrypt(secret.as_bytes(), key)?;
    Ok(hex::encode(encrypted))
}

/// Decrypt a hex-encoded TOTP secret.
pub fn decrypt_secret(encrypted_hex: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    let encrypted = hex::decode(encrypted_hex).map_err(|e| anyhow::anyhow!("Invalid hex: {e}"))?;
    let decrypted = think_watch_common::crypto::decrypt(&encrypted, key)?;
    String::from_utf8(decrypted).map_err(|e| anyhow::anyhow!("Invalid UTF-8: {e}"))
}

fn build_totp(secret_base32: &str, email: &str) -> anyhow::Result<Totp> {
    let secret = Secret::try_from_base32(secret_base32)
        .map_err(|e| anyhow::anyhow!("Invalid TOTP secret: {e}"))?;

    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(DIGITS)
        .with_skew(SKEW)
        .with_step_duration(STEP)
        .with_secret(secret)
        .with_issuer(Some(ISSUER))
        .with_account_name(email)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create TOTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1 secret "12345678901234567890", truncated
    /// to our six digits. Enrolled authenticators keep the codes they
    /// produce today, so a library upgrade must not change them.
    const RFC6238_SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn rfc6238_vectors() {
        let totp = build_totp(RFC6238_SECRET_B32, "kat@example.com").unwrap();
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(totp.generate(time).to_string(), code, "t={time}");
            assert!(totp.check(code, time).is_some());
        }
        // One step of skew either side, not two.
        assert!(totp.check("081804", 1_111_111_109 + 30).is_some());
        assert!(totp.check("081804", 1_111_111_109 + 60).is_none());
    }

    #[test]
    fn generate_and_verify() {
        let secret = generate_secret();
        let totp = build_totp(&secret, "test@example.com").unwrap();
        let code = totp.generate_current().to_string();
        assert!(verify(&secret, &code, "test@example.com").unwrap());
        assert!(!verify(&secret, "000000", "test@example.com").unwrap());
    }

    #[test]
    fn otpauth_uri_format() {
        let secret = generate_secret();
        let uri = otpauth_uri(&secret, "user@test.com").unwrap();
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains("ThinkWatch"));
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
