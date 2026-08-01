//! Fixed-window counters that cannot lose their expiry.
//!
//! Every Redis-backed rate limit and lockout counter in this codebase
//! is a fixed window: increment a key, compare against a threshold, let
//! it expire. The obvious spelling of that has a hole, and it bit a
//! real deployment:
//!
//! ```ignore
//! SET key 0 EX 60 NX   // no-op when the key already exists
//! INCR key             // recreates it with NO TTL if it expired in between
//! ```
//!
//! When the key expires in the window between those two commands, the
//! `INCR` resurrects it with no expiry at all. From then on the counter
//! only grows, and the subject is locked out **permanently** — waiting
//! doesn't help, because nothing will ever clear it. On the
//! signing-key endpoint that meant an operator who tripped the limit
//! could never register a client key again, and so could never use the
//! console: every request failed signature verification, the app
//! retried the login, and the retry burned the limit further.
//!
//! The variant that increments first and sets the TTL only when the
//! counter comes back as 1 has the same end state: a key that is
//! already TTL-less never returns 1 again, so its expiry is never
//! restored.
//!
//! [`incr`] closes both: it increments, then applies the TTL with
//! `EXPIRE … NX`, which sets an expiry only when the key has none. That
//! is idempotent for a healthy window and self-healing for a key that
//! somehow lost its TTL — including ones stranded by the old code.

use fred::clients::Client;
use fred::error::Error;
use fred::interfaces::KeysInterface;
use fred::types::ExpireOptions;

/// Increment `key`'s counter and return the new value, guaranteeing the
/// key expires within `ttl_secs`.
///
/// The TTL is applied after the increment rather than before, so a
/// window that is already running keeps its original deadline — this is
/// a fixed window, not a sliding one, and refreshing the TTL on every
/// hit would let a steady stream of requests hold a counter open
/// forever.
pub async fn incr(client: &Client, key: &str, ttl_secs: i64) -> Result<u64, Error> {
    let count: u64 = client.incr_by(key, 1).await?;
    // NX ⇒ only when the key has no expiry: the common case is a no-op,
    // and the uncommon case is exactly the bug this exists to prevent.
    let _: () = client
        .expire(key, ttl_secs, Some(ExpireOptions::NX))
        .await?;
    Ok(count)
}
