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

// Argon2 costs tens of milliseconds of CPU on purpose. Called from a
// request handler it would hold an async worker for that long, stalling
// every other request scheduled on that thread, so the handlers use
// these versions, which run it on the blocking pool.

/// Share CPU admission with the data plane and limit Argon2 memory using
/// the container allowance. The running task owns both places even when
/// its caller is cancelled.
async fn on_blocking_pool<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let permit = think_watch_common::resources::password_slot().await;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
}

/// [`hash_password`] on the blocking pool.
pub async fn hash_password_off_runtime(password: String) -> anyhow::Result<String> {
    on_blocking_pool(move || hash_password(&password))
        .await
        .map_err(|e| anyhow::anyhow!("Password hashing task failed: {e}"))?
}

/// [`verify_password`] on the blocking pool.
pub async fn verify_password_off_runtime(password: String, hash: String) -> anyhow::Result<bool> {
    on_blocking_pool(move || verify_password(&password, &hash))
        .await
        .map_err(|e| anyhow::anyhow!("Password verification task failed: {e}"))?
}

/// [`verify_password_or_dummy`] on the blocking pool. A task that cannot
/// run counts as a mismatch, like an unreadable hash.
pub async fn verify_password_or_dummy_off_runtime(password: String, hash: Option<String>) -> bool {
    on_blocking_pool(move || verify_password_or_dummy(&password, hash.as_deref()))
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_callers_does_not_admit_more_running_password_jobs() {
        use std::time::Duration;
        let policy = think_watch_common::resources::policy();
        let jobs = policy.argon_jobs.min(policy.cpu_jobs);
        let (started, mut beginnings) = tokio::sync::mpsc::unbounded_channel();
        let mut release = Vec::new();
        let mut callers = Vec::new();
        for _ in 0..jobs {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            release.push(tx);
            let started = started.clone();
            callers.push(tokio::spawn(on_blocking_pool(move || {
                started.send(()).unwrap();
                let _ = rx.recv_timeout(Duration::from_secs(10));
            })));
        }
        for _ in 0..jobs {
            tokio::time::timeout(Duration::from_secs(5), beginnings.recv())
                .await
                .unwrap()
                .unwrap();
        }
        for caller in callers {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        let next = tokio::spawn(on_blocking_pool(move || started.send(()).unwrap()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), beginnings.recv())
                .await
                .is_err()
        );
        drop(release);
        tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(beginnings.recv().await, Some(()));
    }

    #[tokio::test]
    async fn the_off_runtime_versions_agree_with_the_blocking_ones() {
        let hash = hash_password_off_runtime("pw".into()).await.unwrap();
        assert!(
            verify_password_off_runtime("pw".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(
            !verify_password_off_runtime("nope".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(verify_password_or_dummy_off_runtime("pw".into(), Some(hash)).await);
        assert!(!verify_password_or_dummy_off_runtime("pw".into(), None).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_more_runs_at_once_than_there_are_permits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);
        let permits = think_watch_common::resources::policy()
            .argon_jobs
            .min(think_watch_common::resources::policy().cpu_jobs);
        let tasks: Vec<_> = (0..permits * 4)
            .map(|_| {
                tokio::spawn(on_blocking_pool(|| {
                    let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
                    PEAK.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    RUNNING.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let peak = PEAK.load(Ordering::SeqCst);
        assert!(
            (1..=permits).contains(&peak),
            "peak {peak}, permits {permits}"
        );
    }

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
