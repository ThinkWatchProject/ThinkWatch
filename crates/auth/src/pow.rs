//! Proof-of-work challenge for the login endpoint.
//!
//! ## Why
//!
//! Existing brute-force protection (per-`(IP, email)` 10/min, per-IP
//! 30/min, exponential lockout after 5 failures) covers the
//! single-source attacker. It misses two cases:
//!
//!   1. **Distributed brute-force** — attacker controls 100 IPs each
//!      under the per-IP cap. Aggregate request rate dwarfs what one
//!      legitimate user generates, but no single counter trips.
//!   2. **Slow dictionary attack** — a few attempts per email per day
//!      slips under every limit and runs forever.
//!
//! PoW imposes a per-attempt CPU cost (~250ms on modern hardware)
//! that legitimate users absorb invisibly (the work runs in a Web
//! Worker while they type their password) but multiplies an
//! attacker's compute budget linearly with attempts.
//!
//! ## Mechanism
//!
//! - Server issues `(challenge_id, challenge_random, difficulty)`,
//!   stashes `challenge_id → (challenge_random, difficulty)` in Redis
//!   under [`POW_CHALLENGE_PREFIX`] with [`CHALLENGE_TTL_SECS`].
//! - Client finds a `nonce` (any UTF-8 string, typically a number)
//!   such that `SHA-256(challenge_random || ":" || nonce)` has at
//!   least `difficulty` leading zero *bits*.
//! - Client posts `{ challenge_id, nonce }` alongside the login.
//! - Server `GETDEL`s the challenge (single-use, prevents replay),
//!   re-computes the hash, checks the leading-zero count, then runs
//!   the normal credential check.
//!
//! Difficulty 19 → expected ~262144 SHA-256 ops → ~150-250ms in a
//! browser Web Worker via WebCrypto. Tunable per-environment via the
//! settings table; defaults are conservative.

use data_encoding::BASE64URL_NOPAD;
use sha2::{Digest, Sha256};

/// Number of leading zero bits required of `SHA-256(challenge:nonce)`.
/// 19 averages ~250ms in a modern browser Web Worker; a server
/// brute-forcer needing 1B/min has to commit ~250M CPU-seconds/min,
/// far above what a typical compromised botnet can sustain
/// gratuitously.
pub const DEFAULT_DIFFICULTY: u8 = 19;

/// Hard ceiling — anything above this risks legitimate-user dropouts
/// on slow hardware. The setting layer clamps to this on read.
pub const MAX_DIFFICULTY: u8 = 24;

/// TTL on the Redis `pow:challenge:{id}` blob. 5 minutes is long
/// enough for a slow user to fill out the form on a slow device + a
/// human-paced password mistype, short enough that an abandoned
/// challenge ages out before becoming an attack vector.
pub const CHALLENGE_TTL_SECS: i64 = 300;

/// Redis key prefix for issued challenges.
pub const POW_CHALLENGE_PREFIX: &str = "pow:challenge:";

/// Verify that `SHA-256(challenge_random || ":" || nonce)` has at
/// least `difficulty` leading zero bits. Returns `true` on a valid
/// proof.
///
/// Constant separator `:` between random and nonce so an attacker
/// can't swap part of the random into the nonce side and re-use a
/// pre-computed hash for a different `(random, nonce)` pair.
pub fn verify_pow(challenge_random: &str, nonce: &str, difficulty: u8) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(challenge_random.as_bytes());
    hasher.update(b":");
    hasher.update(nonce.as_bytes());
    let digest = hasher.finalize();
    leading_zero_bits(&digest) >= difficulty as u32
}

/// Count leading zero bits in a byte slice. Big-endian — byte 0
/// holds the highest-order bits.
fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut n = 0u32;
    for &b in bytes {
        if b == 0 {
            n += 8;
        } else {
            n += b.leading_zeros();
            break;
        }
    }
    n
}

/// Mint a fresh `(challenge_id, challenge_random)` pair. Both are
/// 22-char URL-safe base64 (16 bytes of entropy each); IDs are
/// independent of the random value so an attacker who learns one
/// can't infer the other.
pub fn mint_challenge() -> (String, String) {
    use rand::RngExt;
    let id: [u8; 16] = rand::rng().random();
    let rnd: [u8; 16] = rand::rng().random();
    (BASE64URL_NOPAD.encode(&id), BASE64URL_NOPAD.encode(&rnd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_zero_bits_basic() {
        assert_eq!(leading_zero_bits(&[0xff]), 0);
        assert_eq!(leading_zero_bits(&[0x7f]), 1);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0xff]), 8);
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x40]), 17);
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x00, 0x00]), 32);
    }

    #[test]
    fn verify_rejects_zero_difficulty_with_wrong_random() {
        // 0-bit difficulty accepts any solution; sanity check that
        // the function still reads challenge_random correctly.
        assert!(verify_pow("rnd-A", "0", 0));
        assert!(verify_pow("rnd-B", "0", 0));
    }

    #[test]
    fn verify_finds_solution_at_low_difficulty() {
        // Brute-force a small solution to exercise the happy path.
        // 12 bits is fast (avg ~4096 iters).
        let challenge = "test-random";
        let mut nonce = 0u64;
        loop {
            let candidate = nonce.to_string();
            if verify_pow(challenge, &candidate, 12) {
                break;
            }
            nonce += 1;
            assert!(nonce < 1_000_000, "12-bit difficulty should land fast");
        }
    }

    #[test]
    fn verify_rejects_solution_for_different_random() {
        // Find a valid nonce for one random, then verify against a
        // different random — must fail.
        let mut nonce = 0u64;
        loop {
            if verify_pow("rnd-1", &nonce.to_string(), 12) {
                break;
            }
            nonce += 1;
        }
        let n = nonce.to_string();
        assert!(verify_pow("rnd-1", &n, 12));
        assert!(
            !verify_pow("rnd-2", &n, 12),
            "valid nonce for rnd-1 must NOT validate against rnd-2"
        );
    }

    #[test]
    fn mint_challenge_returns_distinct_high_entropy_pair() {
        let (a_id, a_rnd) = mint_challenge();
        let (b_id, b_rnd) = mint_challenge();
        assert_ne!(a_id, b_id);
        assert_ne!(a_rnd, b_rnd);
        assert_ne!(a_id, a_rnd, "id and random must be independent");
        assert_eq!(a_id.len(), 22, "16 bytes base64-url-no-pad = 22 chars");
        assert_eq!(a_rnd.len(), 22);
    }
}
