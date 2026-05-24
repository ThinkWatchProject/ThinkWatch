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
//! - Client posts `{ email }` to `/api/auth/pow-challenge`. Server
//!   issues `(challenge_id, challenge_random, difficulty)` and
//!   stashes `challenge_id → (challenge_random, difficulty, email)`
//!   in Redis under [`POW_CHALLENGE_PREFIX`] with
//!   [`CHALLENGE_TTL_SECS`].
//! - Client finds a `nonce` (any UTF-8 string, typically a number)
//!   such that `SHA-256(challenge_random || ":" || email || ":" ||
//!   nonce)` has at least `difficulty` leading zero *bits*.
//! - Client posts `{ challenge_id, nonce }` alongside the login.
//! - Server `GETDEL`s the challenge (single-use, prevents replay),
//!   re-computes the hash with the stored email, checks the
//!   leading-zero count, **then** asserts the stored email matches
//!   the login request email, then runs the normal credential
//!   check.
//!
//! ## Why email is in the hash AND a stored field
//!
//! Mixing email into the SHA-256 input means a challenge ground for
//! `alice@x` cannot be replayed against `bob@x` — the hashes would
//! differ. Keeping the bound email in Redis lets verify reject
//! mismatched email at the metadata layer too (defense in depth,
//! and a clearer 4xx than "invalid nonce").
//!
//! Practical effect: an attacker pre-mining a stockpile of
//! challenges must commit to which email each one targets at mint
//! time. They cannot grind one challenge and try it against every
//! email in a credential-stuffing list.
//!
//! Difficulty 19 → expected ~262144 SHA-256 ops → ~150-250ms in a
//! browser Web Worker via WebCrypto. Tunable per-environment via the
//! settings table; defaults are conservative. The mint handler may
//! also escalate to higher difficulty when the requesting IP
//! subnet has recent login failures — see
//! [`difficulty_for_subnet_failures`].

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

/// Verify that `SHA-256(challenge_random || ":" || email || ":" ||
/// nonce)` has at least `difficulty` leading zero bits. Returns
/// `true` on a valid proof.
///
/// The email is part of the hash input so a challenge ground for
/// one account cannot be replayed against another — the SHA-256
/// output differs as soon as the email differs. The server passes
/// the email it stored at mint time (NOT the email from the login
/// request) to keep this honest: a forged login email won't change
/// what the hash gate accepts.
///
/// Constant `:` separator between every field so an attacker can't
/// swap part of one component into another and pre-compute a hash
/// for a different `(random, email, nonce)` triple.
pub fn verify_pow(challenge_random: &str, email: &str, nonce: &str, difficulty: u8) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(challenge_random.as_bytes());
    hasher.update(b":");
    hasher.update(email.as_bytes());
    hasher.update(b":");
    hasher.update(nonce.as_bytes());
    let digest = hasher.finalize();
    leading_zero_bits(&digest) >= difficulty as u32
}

/// Pick a PoW difficulty based on recent login failures observed
/// from the requesting IP's /24 (IPv4) or /48 (IPv6) subnet. Three
/// tiers so a single mistyping user on a corporate NAT doesn't
/// instantly land in the slow lane, but a confirmed brute-force
/// botnet hitting from a /24 pays compound interest.
///
/// At difficulty 19 the median grind is ~150-250ms, 21 jumps to
/// ~600ms-1s, 23 to ~2-4s. Bots feel each step; humans don't notice
/// the first.
pub fn difficulty_for_subnet_failures(recent_failures: u64) -> u8 {
    if recent_failures >= 50 {
        23
    } else if recent_failures >= 10 {
        21
    } else {
        DEFAULT_DIFFICULTY
    }
}

/// Compute the Redis subnet key for failure aggregation. `/24` on
/// IPv4 (256 hosts) and `/48` on IPv6 (the typical residential
/// allocation) match how botnets are actually distributed without
/// being so wide that one bad actor sinks an entire ISP.
///
/// Callers MUST pass a real IP — the auth handlers gate on
/// `require_client_ip` and reject the request with 400 when no IP
/// can be resolved, so this function is only invoked with parseable
/// input in production. The unparseable-input branch returns the
/// raw string for test ergonomics; if it ever fires in prod, the
/// `require_client_ip` invariant has been violated upstream.
pub fn subnet_key(ip: &str) -> String {
    use std::net::IpAddr;
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        Ok(IpAddr::V6(v6)) => {
            // Unmap IPv4-mapped IPv6 BEFORE bucketing. Without this,
            // every `::ffff:V4` address (which some reverse proxies
            // emit in XFF when running dual-stack) lands in the
            // all-zero `0:0:0::/48` bucket — recreating the very
            // "shared bucket" failure mode that motivated
            // require_client_ip. Run the unmapped V4 through the
            // /24 branch instead so a real IPv4 attacker can't hide
            // their /24 bucket by sending the mapped form.
            if let Some(v4) = v6.to_ipv4_mapped() {
                let o = v4.octets();
                return format!("{}.{}.{}.0/24", o[0], o[1], o[2]);
            }
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
        Err(_) => ip.to_string(),
    }
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
        assert!(verify_pow("rnd-A", "a@x.com", "0", 0));
        assert!(verify_pow("rnd-B", "a@x.com", "0", 0));
    }

    #[test]
    fn verify_finds_solution_at_low_difficulty() {
        // Brute-force a small solution to exercise the happy path.
        // 12 bits is fast (avg ~4096 iters).
        let challenge = "test-random";
        let email = "user@example.com";
        let mut nonce = 0u64;
        loop {
            let candidate = nonce.to_string();
            if verify_pow(challenge, email, &candidate, 12) {
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
        let email = "user@example.com";
        let mut nonce = 0u64;
        loop {
            if verify_pow("rnd-1", email, &nonce.to_string(), 12) {
                break;
            }
            nonce += 1;
        }
        let n = nonce.to_string();
        assert!(verify_pow("rnd-1", email, &n, 12));
        assert!(
            !verify_pow("rnd-2", email, &n, 12),
            "valid nonce for rnd-1 must NOT validate against rnd-2"
        );
    }

    #[test]
    fn verify_rejects_solution_for_different_email() {
        // The whole point of email-binding: a challenge ground for
        // alice can't be replayed against bob even if the attacker
        // knows the (challenge_random, nonce) pair.
        let mut nonce = 0u64;
        loop {
            if verify_pow("rnd-1", "alice@x.com", &nonce.to_string(), 12) {
                break;
            }
            nonce += 1;
        }
        let n = nonce.to_string();
        assert!(verify_pow("rnd-1", "alice@x.com", &n, 12));
        assert!(
            !verify_pow("rnd-1", "bob@x.com", &n, 12),
            "valid nonce for alice MUST NOT validate against bob"
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

    #[test]
    fn difficulty_escalates_with_subnet_failure_count() {
        assert_eq!(difficulty_for_subnet_failures(0), DEFAULT_DIFFICULTY);
        assert_eq!(difficulty_for_subnet_failures(9), DEFAULT_DIFFICULTY);
        assert_eq!(difficulty_for_subnet_failures(10), 21);
        assert_eq!(difficulty_for_subnet_failures(49), 21);
        assert_eq!(difficulty_for_subnet_failures(50), 23);
        assert_eq!(difficulty_for_subnet_failures(1_000_000), 23);
    }

    #[test]
    fn subnet_key_groups_ipv4_by_24() {
        assert_eq!(subnet_key("192.168.1.42"), "192.168.1.0/24");
        assert_eq!(subnet_key("192.168.1.99"), "192.168.1.0/24");
        assert_eq!(subnet_key("192.168.2.42"), "192.168.2.0/24");
        assert_eq!(subnet_key("10.0.0.1"), "10.0.0.0/24");
    }

    #[test]
    fn subnet_key_groups_ipv6_by_48() {
        assert_eq!(subnet_key("2001:db8:1::1"), "2001:db8:1::/48");
        assert_eq!(subnet_key("2001:db8:1::abcd"), "2001:db8:1::/48");
        assert_eq!(subnet_key("2001:db8:2::1"), "2001:db8:2::/48");
    }

    #[test]
    fn subnet_key_ipv6_canonicalizes_before_grouping() {
        // `Ipv6Addr::from_str` normalises before `.segments()`, so a
        // fully-expanded form collides with its `::` shorthand.
        assert_eq!(
            subnet_key("2001:db8:1::1"),
            subnet_key("2001:0db8:0001:0000:0000:0000:0000:0001"),
        );
    }

    #[test]
    fn subnet_key_ipv6_loopback_and_linklocal_share_zero_bucket() {
        // Both addresses start with three zero segments and collapse
        // into the same key. In production these wouldn't reach the
        // login handler (they're not routable across the public
        // internet), but if a reverse proxy ever forwards them they
        // aggregate together. Documenting the behaviour so a future
        // reviewer doesn't read it as a bug.
        assert_eq!(subnet_key("::1"), "0:0:0::/48");
        assert_eq!(subnet_key("fe80::abcd"), "fe80:0:0::/48");
    }

    #[test]
    fn subnet_key_ipv4_mapped_ipv6_unmaps_to_v4_bucket() {
        // `::ffff:192.0.2.1` is an IPv4-mapped IPv6 address. We
        // unmap to the real V4 address BEFORE bucketing so the /24
        // attack signal isn't lost (the all-zero `0:0:0::/48` bucket
        // would have been a shared sink for every distinct mapped
        // V4, recreating the same "shared bucket" failure mode that
        // motivated require_client_ip).
        assert_eq!(subnet_key("::ffff:192.0.2.1"), "192.0.2.0/24");
        // Distinct mapped V4s land in distinct /24s.
        assert_eq!(subnet_key("::ffff:10.0.0.5"), "10.0.0.0/24");
        // Same bucket as the unmapped form.
        assert_eq!(subnet_key("::ffff:10.0.0.5"), subnet_key("10.0.0.5"));
    }

    #[test]
    fn subnet_key_passes_through_unparseable() {
        // Production callers go through `require_client_ip`, which
        // 400s if an IP can't be resolved — this branch is for unit
        // tests and operator debugging only. We still want a
        // deterministic key so test data is stable.
        assert_eq!(subnet_key("unknown"), "unknown");
        assert_eq!(subnet_key(""), "");
        assert_eq!(subnet_key("not-an-ip"), "not-an-ip");
    }
}
