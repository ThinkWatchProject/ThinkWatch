//! Moved to thinkwatch-core (`tw-crypto`). See `crypto.rs` for why.
//!
//! The one deliberate difference: core returns its own `SecretError`
//! instead of this crate's `AppError` — the shared layer must not know
//! our error taxonomy. `From<SecretError> for AppError` lives in
//! `errors.rs`, so `?` at every call site keeps working untouched.

pub use tw_crypto::json_secret::*;
