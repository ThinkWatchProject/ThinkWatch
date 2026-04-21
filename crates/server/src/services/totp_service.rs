//! TOTP service — the few bits of TOTP plumbing the handlers all
//! duplicate: parsing the encryption key from `AppConfig`, and
//! round-tripping a secret through AES-256-GCM.
//!
//! The cryptographic primitives live in `think_watch_auth::totp`; this
//! module just wires them to `AppState.config.encryption_key` so the
//! handlers don't repeat the `parse_encryption_key(...) → encrypt/decrypt`
//! dance four times.

use think_watch_common::crypto::parse_encryption_key;
use think_watch_common::errors::AppError;

use crate::app::AppState;

/// Parse the 32-byte encryption key out of the running config, mapping
/// the low-level crypto error into the handler-facing `AppError`
/// variant.
fn encryption_key(state: &AppState) -> Result<[u8; 32], AppError> {
    parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption key error: {e}")))
}

/// Encrypt a plaintext TOTP secret for at-rest storage. Returns the
/// hex-encoded ciphertext ready to bind into `users.totp_secret`.
pub(crate) fn encrypt_secret(state: &AppState, secret_plaintext: &str) -> Result<String, AppError> {
    let key = encryption_key(state)?;
    think_watch_auth::totp::encrypt_secret(secret_plaintext, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("TOTP encrypt error: {e}")))
}

/// Recover the plaintext TOTP secret from a stored `users.totp_secret`.
pub(crate) fn decrypt_secret(state: &AppState, encrypted_hex: &str) -> Result<String, AppError> {
    let key = encryption_key(state)?;
    think_watch_auth::totp::decrypt_secret(encrypted_hex, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("TOTP decrypt error: {e}")))
}
