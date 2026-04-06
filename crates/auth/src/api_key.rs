use hmac::{Hmac, Mac, digest::KeyInit};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const KEY_PREFIX: &str = "tw-";
const KEY_LENGTH: usize = 48;

/// Returns true if the given token looks like a ThinkWatch API key.
/// Accepts both the current `tw-` prefix and the legacy `ab-` prefix
/// for backward compatibility with keys issued before the project rename.
pub fn is_api_key(token: &str) -> bool {
    token.starts_with(KEY_PREFIX) || token.starts_with("ab-")
}

/// Server-side HMAC key for API key hashing.
/// This ensures API key hashes cannot be brute-forced without the server secret,
/// and prevents hash collisions between different server instances.
const HMAC_DOMAIN_KEY: &[u8] = b"think-watch-api-key-v1";

pub struct GeneratedApiKey {
    pub plaintext: String,
    pub prefix: String,
    pub hash: String,
}

pub fn generate_api_key() -> GeneratedApiKey {
    let mut random_bytes = vec![0u8; KEY_LENGTH];
    rand::fill(&mut random_bytes[..]);
    let encoded = hex::encode(&random_bytes);

    let plaintext = format!("{KEY_PREFIX}{encoded}");
    let prefix = plaintext[..11].to_string(); // "tw-" + 8 chars
    let hash = hash_api_key(&plaintext);

    GeneratedApiKey {
        plaintext,
        prefix,
        hash,
    }
}

pub fn hash_api_key(key: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(HMAC_DOMAIN_KEY).expect("HMAC accepts any key length");
    mac.update(key.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_api_key(plaintext: &str, stored_hash: &str) -> bool {
    use subtle::ConstantTimeEq;
    let computed = hash_api_key(plaintext);
    computed.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_api_key_has_correct_prefix() {
        let key = generate_api_key();
        assert!(
            key.plaintext.starts_with("tw-"),
            "key should start with tw- prefix"
        );
        assert!(
            key.prefix.starts_with("tw-"),
            "prefix should start with tw-"
        );
        assert_eq!(key.prefix.len(), 11, "prefix should be 11 chars (tw- + 8)");
    }

    #[test]
    fn hash_api_key_is_deterministic() {
        let input = "tw-deadbeef12345678";
        let h1 = hash_api_key(input);
        let h2 = hash_api_key(input);
        assert_eq!(h1, h2, "same input must produce same hash");
    }

    #[test]
    fn verify_api_key_roundtrip() {
        let key = generate_api_key();
        assert!(
            verify_api_key(&key.plaintext, &key.hash),
            "generated key should verify against its own hash"
        );
    }

    #[test]
    fn verify_api_key_rejects_wrong_key() {
        let key = generate_api_key();
        assert!(
            !verify_api_key("tw-wrong_key", &key.hash),
            "wrong plaintext should not verify"
        );
    }

    #[test]
    fn is_api_key_accepts_both_prefixes() {
        assert!(is_api_key("tw-anything"));
        assert!(is_api_key("ab-legacy"));
        assert!(!is_api_key("eyJhbGciOi"));
    }
}
