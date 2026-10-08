use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};

const RANDOM_PASSWORD_LEN: usize = 16;
const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%&*";

/// Generate a cryptographically random password.
pub fn generate_random_password() -> String {
    let mut bytes = [0u8; RANDOM_PASSWORD_LEN];
    rand::fill(&mut bytes);
    bytes
        .iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    // Random 16-byte salt from the OS RNG, Argon2id v19 with the OWASP
    // parameters (m=19456, t=2, p=1).
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("Password hashing failed: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> anyhow::Result<bool> {
    let parsed_hash =
        PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("Invalid password hash: {e}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

/// A well-formed Argon2id hash that no password produces, with the
/// parameters [`hash_password`] uses. Verifying against it costs what
/// verifying a real password does.
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Verify `password` against `hash`, or against [`DUMMY_HASH`] when there
/// is none (no such user, or an account without a password), so that how
/// long a login takes does not say whether the account exists. Never
/// `true` without a hash; a hash that cannot be read counts as a mismatch.
pub fn verify_password_or_dummy(password: &str, hash: Option<&str>) -> bool {
    match hash {
        Some(hash) => verify_password(password, hash).unwrap_or(false),
        None => {
            let _ = verify_password(password, DUMMY_HASH);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hash written by argon2 0.5.3 (`Argon2::default()`, fixed salt).
    /// Stored hashes outlive crate upgrades: they must keep verifying.
    const KAT_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$dGhpbmt3YXRjaC1rYXQtc2FsdA$6xffcgfh1b9KJMWg4mBS4qOWsK5MNUM1asLNHvgV0v8";

    #[test]
    fn stored_hash_from_earlier_release_verifies() {
        assert!(verify_password("correct-horse-battery-staple", KAT_HASH).unwrap());
        assert!(!verify_password("correct-horse-battery-stapler", KAT_HASH).unwrap());
    }

    #[test]
    fn new_hashes_use_argon2id_with_owasp_parameters() {
        let hash = hash_password("pw").unwrap();
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "unexpected parameters: {hash}"
        );
    }

    /// The login handler verifies against [`DUMMY_HASH`] when the account
    /// does not exist. If the dummy stopped parsing, `verify_password` would
    /// fail at once, and a login for an unknown address would answer in
    /// microseconds instead of the tens of milliseconds Argon2 takes: an
    /// oracle for which addresses have accounts.
    #[test]
    fn the_dummy_hash_runs_argon2_at_the_cost_of_a_real_one() {
        // Parsed and computed: Ok(false), not an error.
        assert!(!verify_password("anything", DUMMY_HASH).unwrap());
        // Same algorithm, version and cost parameters as a fresh hash, and
        // an output of the same length.
        let real = hash_password("pw").unwrap();
        let fields = |h: &str| h.split('$').map(str::to_string).collect::<Vec<_>>();
        let (d, r) = (fields(DUMMY_HASH), fields(&real));
        assert_eq!(d.len(), r.len(), "{DUMMY_HASH} vs {real}");
        assert_eq!(d[1..4], r[1..4], "algorithm, version or parameters differ");
        assert_eq!(d[5].len(), r[5].len(), "output length differs");
        // And the helper the login uses never accepts without a hash.
        assert!(!verify_password_or_dummy("anything", None));
        assert!(!verify_password_or_dummy("", None));
        assert!(verify_password_or_dummy("pw", Some(&real)));
        assert!(!verify_password_or_dummy("pw", Some("not a hash")));
    }

    #[test]
    fn hash_and_verify_roundtrip() {
        let password = "correct-horse-battery-staple";
        let hash = hash_password(password).expect("hashing should succeed");
        let ok = verify_password(password, &hash).expect("verify should succeed");
        assert!(ok, "correct password should verify");
    }

    #[test]
    fn wrong_password_returns_false() {
        let hash = hash_password("real-password").expect("hashing should succeed");
        let ok = verify_password("wrong-password", &hash).expect("verify should succeed");
        assert!(!ok, "wrong password should not verify");
    }

    #[test]
    fn hash_is_not_plaintext() {
        let password = "my-secret-password";
        let hash = hash_password(password).expect("hashing should succeed");
        assert_ne!(hash, password, "hash must not be the plaintext password");
        assert!(hash.starts_with("$argon2"), "hash should be argon2 format");
    }
}
