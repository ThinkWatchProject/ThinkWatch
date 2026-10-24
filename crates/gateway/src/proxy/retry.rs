//! Sending a request again after the upstream failed for the moment.
//!
//! When every candidate route has been tried and the last one failed for
//! the moment, and none of the answer has reached the client, the request
//! waits and goes again to the routes that failed that way, instead of
//! handing the failure to the client. A model with one route — and a
//! stream, which is sent to the one route it picked — sends to it again.
//!
//! **For the moment** ([`transient`]): overloaded (503, 529, and the same
//! errors reported inside a stream), a server error (500, 502, 504), a
//! connection that could not be made or broke before the answer's head, a
//! stream that broke off before any of the answer. **Not** sent again: an
//! upstream's rate limit or spent quota (429), a refused credential (401,
//! 403), any other 4xx (a bad request, a model the upstream does not
//! have), a timeout, the gateway's own limits, budgets and policies, and a
//! client that left. Neither is anything after part of the answer went
//! out: the client would get its beginning twice.
//!
//! **How long it waits** ([`Budget`]): 2, 4, 8, then 16 seconds each
//! time, give or take a fifth so that many requests failing together do
//! not come back together; or as long as the upstream said to wait
//! (`retry-after-ms`, `Retry-After`) — but never less than
//! [`MIN_SAID_WAIT`]: an upstream that says "now" would otherwise be
//! asked again at once, hundreds of times over. It stops when the wait
//! would end past the request's deadline, `gateway.retry_wait_secs` after
//! the request arrived (0: never sent again). There is no count of its
//! own. An attempt already under way is not cut short by that deadline:
//! it only decides whether another one starts.
//!
//! A whole (non-streamed) answer has a second, hard deadline: the
//! gateway's request timeout (`super::REQUEST_TIMEOUT`), after which the
//! client gets an empty 408 and the request no log row. Each attempt is
//! cut at it, and a retry starts only when the wait and another attempt
//! as long as the last one fit before it (see [`Budget::within`]).
//!
//! Each attempt is a pick like any other: the strategy, the breaker, the
//! route caps, the session affinity and the provider holding a
//! conversation's encrypted history (`super::sealed`) all apply, and the
//! failed attempt counts against its route as before.

use std::time::{Duration, Instant};

use rand::RngExt;

use crate::error::GatewayError;

/// What the client receives while the gateway holds back the opening of
/// a stream or waits to send again: an SSE comment, which every SSE
/// reader skips. Sent once nothing has gone out for [`KEEP_ALIVE`].
pub(crate) const KEEP_ALIVE_FRAME: &[u8] = b": keep-alive\n\n";

/// How long the client may go without a byte while the gateway holds a
/// stream back.
pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Did the upstream fail for the moment, so that the same request may
/// well be answered a little later? From what a stream records of its
/// error, as [`super::routing::fails`] reads it: the error's tag and
/// status.
///
/// `transport` is a stream that broke off in transit (see
/// `lifecycle::broke_off`). `NetworkError` is a connection that could not
/// be made, a TLS failure, or one that broke before or while the answer
/// was read. A timeout (`ProviderTimeout`) is not: the upstream already
/// had all that time.
pub(crate) fn transient(error_tag: &str, status: i64) -> bool {
    match error_tag {
        "ProviderHttpError" => matches!(status, 500 | 502 | 503 | 504 | 529),
        "NetworkError" | "transport" => true,
        _ => false,
    }
}

/// [`transient`] for an error.
pub(crate) fn is_transient(err: &GatewayError) -> bool {
    transient(err.error_tag(), err.status_code())
}

/// How long before the gateway's request timeout a whole answer's last
/// attempt is cut: time to write its error and its log row.
pub(crate) const TIMEOUT_MARGIN: Duration = Duration::from_secs(2);

/// The hard deadline of a whole answer to a request that `arrived` at the
/// gateway port (see [`super::Arrived`]).
pub(crate) fn hard_deadline(arrived: Instant) -> Instant {
    arrived + super::REQUEST_TIMEOUT.saturating_sub(TIMEOUT_MARGIN)
}

/// The shortest wait an upstream's `retry-after-ms` / `Retry-After` is
/// taken for. Zero is taken as no word at all: the backoff applies.
pub(crate) const MIN_SAID_WAIT: Duration = Duration::from_millis(250);

/// How long the upstream said to wait before trying again, held to
/// [`MIN_SAID_WAIT`].
fn said_to_wait(err: &GatewayError) -> Option<Duration> {
    match err {
        GatewayError::ProviderHttpError { retry_after, .. } => retry_after
            .filter(|d| !d.is_zero())
            .map(|d| d.max(MIN_SAID_WAIT)),
        _ => None,
    }
}

/// The wait before the `n`-th retry (1-based), before jitter: 2, 4, 8,
/// then 16 seconds.
fn backoff(n: u32) -> Duration {
    Duration::from_secs(2u64 << n.clamp(1, 4).saturating_sub(1))
}

/// A request's allowance for sending again: its deadline, and how many
/// retries went out.
#[derive(Debug, Clone)]
pub(crate) struct Budget {
    /// `None`: never sent again.
    deadline: Option<Instant>,
    /// When the answer must be back by, whatever the retries: a whole
    /// answer's request timeout. `None` for a stream, whose body is not
    /// bound by it.
    hard: Option<Instant>,
    retries: u32,
}

impl Budget {
    /// The allowance of a request that arrived at `arrived`: `wait` from
    /// then. Zero: none.
    pub(crate) fn new(arrived: Instant, wait: Duration) -> Self {
        Self {
            deadline: (!wait.is_zero())
                .then(|| arrived.checked_add(wait))
                .flatten(),
            hard: None,
            retries: 0,
        }
    }

    /// The same allowance for a request that must be answered by `hard`:
    /// each attempt is cut there ([`Budget::hard_deadline`]), and a retry
    /// starts only when its wait and another attempt as long as the last
    /// one fit before it.
    pub(crate) fn within(mut self, hard: Instant) -> Self {
        self.hard = Some(hard);
        self
    }

    /// When the attempt under way must be given up, if ever.
    pub(crate) fn hard_deadline(&self) -> Option<Instant> {
        self.hard
    }

    /// Whether a failure now could still be followed by another attempt.
    pub(crate) fn open(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() < d)
    }

    /// How long to wait before sending again after `err`. `None`: `err`
    /// is the answer — it is not transient, or the wait would end past
    /// the deadline.
    pub(crate) fn wait_after(&self, err: &GatewayError) -> Option<Duration> {
        self.wait_after_taking(err, Duration::ZERO)
    }

    /// [`Budget::wait_after`] for an attempt that took `took`: under a
    /// hard deadline, the next one is expected to take as long.
    pub(crate) fn wait_after_taking(&self, err: &GatewayError, took: Duration) -> Option<Duration> {
        if !is_transient(err) {
            return None;
        }
        self.wait(said_to_wait(err), took)
    }

    /// [`Budget::wait_after`] for what a stream recorded of its error.
    pub(crate) fn wait_after_outcome(&self, error_tag: &str, status: i64) -> Option<Duration> {
        if !transient(error_tag, status) {
            return None;
        }
        self.wait(None, Duration::ZERO)
    }

    fn wait(&self, said: Option<Duration>, took: Duration) -> Option<Duration> {
        let jitter = rand::rng().random_range(0.8..=1.2);
        self.wait_at(Instant::now(), said, jitter, took)
    }

    /// The wait before the next retry, `jitter` the factor the backoff is
    /// spread by: the upstream's own word if it gave one, else the
    /// backoff. `None` when it would end past the deadline, or when it and
    /// another attempt as long as the last (`took`) would not fit before
    /// the hard deadline.
    fn wait_at(
        &self,
        now: Instant,
        said: Option<Duration>,
        jitter: f64,
        took: Duration,
    ) -> Option<Duration> {
        let deadline = self.deadline?;
        let wait = said.unwrap_or_else(|| backoff(self.retries + 1).mul_f64(jitter));
        let resumes = now.checked_add(wait)?;
        if resumes > deadline {
            return None;
        }
        if let Some(hard) = self.hard
            && resumes.checked_add(took)? > hard
        {
            return None;
        }
        Some(wait)
    }

    /// The next attempt went out.
    pub(crate) fn sent(&mut self) {
        self.retries += 1;
    }

    /// How many retries went out.
    pub(crate) fn retries(&self) -> u32 {
        self.retries
    }
}

/// `gateway_provider_retry_total{provider}`: an attempt that went out
/// again after a transient failure, by the provider it went to. Next to
/// `gateway_provider_fallback_total`, which counts moving on to another
/// route.
pub(crate) fn count(provider: &str) {
    metrics::counter!(
        "gateway_provider_retry_total",
        "provider" => crate::metrics_labels::normalize_provider_label(provider),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, retry_after: Option<Duration>) -> GatewayError {
        GatewayError::ProviderHttpError {
            status,
            message: "p: down".into(),
            retry_after,
        }
    }

    #[test]
    fn only_a_failure_of_the_moment_is_transient() {
        for status in [500, 502, 503, 504, 529] {
            assert!(is_transient(&http(status, None)), "{status}");
        }
        for status in [400, 404, 408, 413, 422, 501] {
            assert!(!is_transient(&http(status, None)), "{status}");
        }
        assert!(is_transient(&GatewayError::NetworkError("reset".into())));
        // A stream broken off before any of the answer.
        assert!(transient("transport", 502));

        for e in [
            GatewayError::UpstreamRateLimited {
                retry_after_secs: Some(1),
            },
            GatewayError::UpstreamAuthError {
                status: 401,
                message: String::new(),
            },
            GatewayError::ProviderTimeout("slow".into()),
            GatewayError::ProviderInvalidResponse("not json".into()),
            GatewayError::ProviderError("signing failed".into()),
            GatewayError::TransformError("bad".into()),
            GatewayError::PolicyBlocked("rule".into()),
            GatewayError::rate_limited("user:requests/1m", 5),
            GatewayError::budget_exhausted("user:budget/monthly", 5),
            GatewayError::SealedHistoryRefused {
                status: 400,
                provider: "p".into(),
                said: "invalid_encrypted_content".into(),
            },
        ] {
            assert!(!is_transient(&e), "{e:?}");
        }
        // A stream the client left, or one that ended normally.
        assert!(!transient("ClientCancelled", 499));
    }

    #[test]
    fn the_backoff_doubles_up_to_sixteen_seconds() {
        let secs: Vec<u64> = (1..=6).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 16, 16]);
    }

    #[test]
    fn the_wait_is_spread_by_a_fifth_either_way() {
        let now = Instant::now();
        let budget = Budget::new(now, Duration::from_secs(120));
        assert_eq!(
            budget.wait_at(now, None, 0.8, Duration::ZERO),
            Some(Duration::from_millis(1600))
        );
        assert_eq!(
            budget.wait_at(now, None, 1.2, Duration::ZERO),
            Some(Duration::from_millis(2400))
        );
        for _ in 0..100 {
            let w = budget.wait(None, Duration::ZERO).unwrap();
            assert!(
                w >= Duration::from_millis(1600) && w <= Duration::from_millis(2400),
                "{w:?}"
            );
        }
    }

    #[test]
    fn each_retry_waits_longer() {
        let now = Instant::now();
        let mut budget = Budget::new(now, Duration::from_secs(600));
        let mut waits = Vec::new();
        for _ in 0..5 {
            waits.push(
                budget
                    .wait_at(now, None, 1.0, Duration::ZERO)
                    .unwrap()
                    .as_secs(),
            );
            budget.sent();
        }
        assert_eq!(waits, [2, 4, 8, 16, 16]);
        assert_eq!(budget.retries(), 5);
    }

    #[test]
    fn the_upstreams_own_wait_wins_over_the_backoff() {
        let now = Instant::now();
        let budget = Budget::new(now, Duration::from_secs(120));
        let said = Some(Duration::from_millis(300));
        assert_eq!(budget.wait_at(now, said, 1.2, Duration::ZERO), said);
        assert_eq!(
            budget.wait_at(now, Some(Duration::from_secs(30)), 1.0, Duration::ZERO),
            Some(Duration::from_secs(30))
        );
        // Longer than what is left: the failure is the answer.
        assert_eq!(
            budget.wait_at(now, Some(Duration::from_secs(121)), 1.0, Duration::ZERO),
            None
        );
    }

    /// "Now" is no word at all, and "almost now" is held to a floor: an
    /// upstream that says so must not be asked again hundreds of times.
    #[test]
    fn a_wait_of_nothing_from_the_upstream_is_not_taken_at_its_word() {
        assert_eq!(said_to_wait(&http(503, Some(Duration::ZERO))), None);
        assert_eq!(
            said_to_wait(&http(503, Some(Duration::from_millis(1)))),
            Some(MIN_SAID_WAIT)
        );
        assert_eq!(
            said_to_wait(&http(503, Some(Duration::from_secs(3)))),
            Some(Duration::from_secs(3))
        );
        // Zero falls back to the backoff.
        let budget = Budget::new(Instant::now(), Duration::from_secs(120));
        let w = budget.wait_after(&http(503, Some(Duration::ZERO))).unwrap();
        assert!(w >= Duration::from_millis(1600), "{w:?}");
    }

    /// A whole answer's hard deadline: a retry starts only when its wait
    /// and another attempt as long as the last fit before it.
    #[test]
    fn a_retry_must_fit_before_the_hard_deadline() {
        let arrived = Instant::now();
        let budget = Budget::new(arrived, Duration::from_secs(600))
            .within(arrived + Duration::from_secs(118));
        assert_eq!(
            budget.hard_deadline(),
            Some(arrived + Duration::from_secs(118))
        );
        // A 504 after 60 s, 62 s in: a 2 s wait and another 60 s do not fit.
        let at = arrived + Duration::from_secs(62);
        let minute = Duration::from_secs(60);
        assert_eq!(budget.wait_at(at, None, 1.0, minute), None);
        // A quick failure does.
        assert_eq!(
            budget.wait_at(at, None, 1.0, Duration::from_millis(50)),
            Some(Duration::from_secs(2))
        );
        // Without a hard deadline (a stream), only the soft one counts.
        let stream = Budget::new(arrived, Duration::from_secs(600));
        assert_eq!(stream.hard_deadline(), None);
        assert!(stream.wait_at(at, None, 1.0, minute).is_some());
    }

    #[test]
    fn no_wait_ends_past_the_deadline() {
        let arrived = Instant::now();
        let budget = Budget::new(arrived, Duration::from_secs(10));
        assert!(budget.open());
        // 9 s in, 1 s left: a 2 s backoff does not fit.
        assert_eq!(
            budget.wait_at(arrived + Duration::from_secs(9), None, 1.0, Duration::ZERO),
            None
        );
        assert_eq!(
            budget.wait_at(arrived + Duration::from_secs(8), None, 1.0, Duration::ZERO),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn a_zero_wait_never_sends_again() {
        let now = Instant::now();
        let budget = Budget::new(now, Duration::ZERO);
        assert!(!budget.open());
        assert_eq!(budget.wait_at(now, None, 1.0, Duration::ZERO), None);
        assert_eq!(budget.wait_after(&http(503, None)), None);
    }

    #[test]
    fn a_failure_that_is_not_transient_is_the_answer() {
        let budget = Budget::new(Instant::now(), Duration::from_secs(120));
        assert!(budget.wait_after(&http(503, None)).is_some());
        assert_eq!(budget.wait_after(&http(400, None)), None);
        assert_eq!(
            budget.wait_after(&GatewayError::UpstreamRateLimited {
                retry_after_secs: None
            }),
            None
        );
        assert!(budget.wait_after_outcome("transport", 502).is_some());
        assert_eq!(budget.wait_after_outcome("ProviderHttpError", 400), None);
    }
}
