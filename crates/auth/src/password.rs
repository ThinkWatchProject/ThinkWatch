use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
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
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
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

#[cfg(test)]
mod tests {
    use super::*;

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
