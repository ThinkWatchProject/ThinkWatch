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

/// Encrypt the JSON-serialized recovery-codes blob for at-rest
/// storage. Wraps the same AES-256-GCM primitive `encrypt_secret`
/// uses — recovery codes are full TOTP-bypass tokens and must be
/// at-rest-encrypted with the same envelope as `totp_secret`.
pub(crate) fn encrypt_recovery_codes(
    state: &AppState,
    codes: &[String],
) -> Result<String, AppError> {
    let key = encryption_key(state)?;
    let json = serde_json::to_string(codes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Recovery codes JSON error: {e}")))?;
    think_watch_auth::totp::encrypt_secret(&json, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Recovery codes encrypt error: {e}")))
}

/// Decrypt + parse the at-rest recovery-codes blob.
pub(crate) fn decrypt_recovery_codes(
    state: &AppState,
    encrypted_hex: &str,
) -> Result<Vec<String>, AppError> {
    let key = encryption_key(state)?;
    let json = think_watch_auth::totp::decrypt_secret(encrypted_hex, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Recovery codes decrypt error: {e}")))?;
    serde_json::from_str(&json)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Recovery codes JSON parse error: {e}")))
}

/// Redis key holding the last TOTP time step accepted for a user.
pub(crate) fn last_step_key(user_id: uuid::Uuid) -> String {
    format!("totp_last_step:{user_id}")
}

/// How long the last accepted step is remembered: two of the windows a
/// code stays acceptable in. The record only has to outlive the code it
/// guards; after that, every code it would refuse is refused by the time
/// check anyway.
const LAST_STEP_TTL_SECS: u64 = 2 * think_watch_auth::totp::ACCEPT_WINDOW_SECS;

/// Record `step` as the last TOTP time step accepted for `user_id`, if it
/// is later than the one recorded. Returns `false` when it is not: the
/// code was already used (or an earlier one was, after a later code), and
/// must be refused. One atomic script, so two requests racing with the
/// same code cannot both win.
///
/// Fail-closed like the rest of the login checks: a Redis error is a 5xx,
/// not an accepted code.
pub(crate) async fn claim_step(
    redis: &fred::clients::Client,
    user_id: uuid::Uuid,
    step: u64,
) -> Result<bool, AppError> {
    const CLAIM_LUA: &str = "local last = redis.call('GET', KEYS[1]) \
                             if last and tonumber(last) >= tonumber(ARGV[1]) then return 0 end \
                             redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[2]) \
                             return 1";
    let key = last_step_key(user_id);
    let claimed: i64 = fred::interfaces::LuaInterface::eval(
        redis,
        CLAIM_LUA,
        vec![key.as_str()],
        vec![step.to_string(), LAST_STEP_TTL_SECS.to_string()],
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, key = %key, "Redis TOTP step check failed (fail-closed)");
        AppError::Internal(anyhow::anyhow!("Authentication temporarily unavailable"))
    })?;
    Ok(claimed == 1)
}
