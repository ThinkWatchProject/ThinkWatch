//! Per-route capacity caps: `model_routes.rpm_cap` (requests per minute)
//! and `tpm_cap` (weighted tokens per minute). NULL = unlimited.
//!
//! A route at either cap is skipped for the request the way a
//! circuit-broken route is: the next candidate is tried. A request that
//! finds every remaining candidate at its cap is refused with 429 and a
//! `Retry-After` of when the first of them has room again.
//!
//! Counted with the rate-limit engine (`limits::sliding`): a one-minute
//! sliding window of 60 one-second buckets per counter, in Redis.
//!
//!   * Requests: [`admit`] checks both caps before a request is sent to
//!     the route and, when neither is reached, counts the request — one
//!     atomic step, so concurrent requests on several gateway replicas
//!     cannot all slip under the cap. Every attempt sent counts, a
//!     failover's failed attempts included: the upstream received them.
//!   * Tokens: [`record_tokens`] adds what the request used, weighted
//!     like every other token limit (`limits::weight`), after the
//!     response. A window an answer overshoots refuses the next request.
//!
//! The counters of one route share the Redis Cluster hash tag
//! `{route:<route_id>}` so one script reads and writes both.
//!
//! When Redis fails, `security.rate_limit_fail_closed` decides, as for
//! the users' and keys' limits: failing open, the cap is not applied;
//! failing closed, the request is refused as `rate_limiter_unavailable`.

use fred::clients::Client;
use think_watch_common::limits::RateMetric;
use think_watch_common::limits::sliding::{self, Counter};
use uuid::Uuid;

use crate::error::GatewayError;
use crate::router::RouteEntry;

/// The caps count over one minute.
pub const WINDOW_SECS: i32 = 60;

/// The Redis Cluster hash tag every counter of `route_id` carries.
pub fn hash_tag(route_id: Uuid) -> String {
    format!("{{route:{route_id}}}")
}

/// The Redis key of one of a route's counters.
pub fn counter_key(route_id: Uuid, metric: RateMetric) -> String {
    format!(
        "ratelimit:{}:ai_gateway:route:{route_id}:{}:{WINDOW_SECS}",
        hash_tag(route_id),
        metric.as_str()
    )
}

/// One route's caps, as the selection and the post-response accounting
/// read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteCaps {
    pub route_id: Uuid,
    /// Requests per minute.
    pub rpm: Option<u32>,
    /// Weighted tokens per minute.
    pub tpm: Option<u32>,
}

impl RouteCaps {
    pub fn of(entry: &RouteEntry) -> Self {
        Self {
            route_id: entry.route_id,
            rpm: entry.rpm_cap,
            tpm: entry.tpm_cap,
        }
    }

    /// The counters to check before a request: the request count, which
    /// an admitted request adds one to, and the token count, only read.
    fn counters(&self) -> Vec<(RateMetric, Counter)> {
        let bucket_secs = sliding::bucket_secs(WINDOW_SECS);
        [
            (RateMetric::Requests, self.rpm, 1),
            (RateMetric::Tokens, self.tpm, 0),
        ]
        .into_iter()
        .filter_map(|(metric, cap, charge)| {
            cap.map(|cap| {
                (
                    metric,
                    Counter {
                        key: counter_key(self.route_id, metric),
                        bucket_secs,
                        max_count: i64::from(cap),
                        charge,
                    },
                )
            })
        })
        .collect()
    }
}

/// What [`admit`] decided for a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Under its caps (or uncapped); the request has been counted.
    Admitted,
    /// At a cap: skip the route. `label` names the cap
    /// (`route:requests/1m`, `route:tokens/1m`), `retry_after_secs` is
    /// when it has room again.
    Capped {
        label: String,
        retry_after_secs: u64,
    },
}

fn label(metric: RateMetric) -> String {
    format!("route:{}/1m", metric.as_str())
}

/// Before a request is sent to the route: refuse it if the route is at
/// either cap, otherwise count it. A route with no caps costs no Redis
/// call.
pub async fn admit(
    redis: &Client,
    caps: &RouteCaps,
    fail_closed: bool,
) -> Result<Admission, GatewayError> {
    admit_at(
        redis,
        caps,
        fail_closed,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
}

/// [`admit`] at a given time, in Unix milliseconds — for tests.
pub async fn admit_at(
    redis: &Client,
    caps: &RouteCaps,
    fail_closed: bool,
    now_ms: i64,
) -> Result<Admission, GatewayError> {
    let counters = caps.counters();
    if counters.is_empty() {
        return Ok(Admission::Admitted);
    }
    let plain: Vec<Counter> = counters.iter().map(|(_, c)| c.clone()).collect();
    match sliding::admit_counters_at(redis, &plain, now_ms).await {
        Ok(outcome) if outcome.allowed => Ok(Admission::Admitted),
        Ok(outcome) => {
            let metric = outcome
                .exceeded_index
                .and_then(|i| counters.get(i))
                .map_or(RateMetric::Requests, |(m, _)| *m);
            metrics::counter!("gateway_route_capped_total", "metric" => metric.as_str())
                .increment(1);
            Ok(Admission::Capped {
                label: label(metric),
                retry_after_secs: outcome.retry_after_secs,
            })
        }
        Err(e) if fail_closed => {
            metrics::counter!("gateway_rate_limiter_fail_closed_total").increment(1);
            tracing::error!(
                route_id = %caps.route_id,
                "route cap check failed: {e}; failing closed per security.rate_limit_fail_closed"
            );
            Err(GatewayError::limiter_unavailable(
                "rate_limiter_unavailable",
            ))
        }
        Err(e) => {
            metrics::counter!("gateway_rate_limiter_fail_open_total").increment(1);
            tracing::warn!(
                route_id = %caps.route_id,
                "route cap check failed: {e}; failing open"
            );
            Ok(Admission::Admitted)
        }
    }
}

/// After the response: add the request's weighted tokens to the route's
/// token counter, when the route has a token cap. Errors are logged —
/// the answer has already gone out.
pub async fn record_tokens(redis: &Client, caps: &RouteCaps, weighted_tokens: i64) {
    record_tokens_at(
        redis,
        caps,
        weighted_tokens,
        chrono::Utc::now().timestamp_millis(),
    )
    .await;
}

/// [`record_tokens`] at a given time, in Unix milliseconds — for tests.
pub async fn record_tokens_at(redis: &Client, caps: &RouteCaps, weighted_tokens: i64, now_ms: i64) {
    if caps.tpm.is_none() || weighted_tokens <= 0 {
        return;
    }
    let counter = (
        counter_key(caps.route_id, RateMetric::Tokens),
        sliding::bucket_secs(WINDOW_SECS),
    );
    if let Err(e) = sliding::record_counters_at(redis, &[counter], weighted_tokens, now_ms).await {
        tracing::warn!(route_id = %caps.route_id, "route token accounting failed: {e}");
    }
}

/// The routes a selection skipped at their caps, and what to answer when
/// nothing else could take the request.
#[derive(Debug, Default)]
pub(crate) struct CappedRoutes {
    routes: Vec<Uuid>,
    /// The cap that frees first, and when.
    soonest: Option<(String, u64)>,
}

impl CappedRoutes {
    pub(crate) fn add(&mut self, route_id: Uuid, label: String, retry_after_secs: u64) {
        self.routes.push(route_id);
        if self
            .soonest
            .as_ref()
            .is_none_or(|(_, secs)| retry_after_secs < *secs)
        {
            self.soonest = Some((label, retry_after_secs));
        }
    }

    pub(crate) fn routes(&self) -> &[Uuid] {
        &self.routes
    }

    /// 429 when routes were skipped at their caps: try again when the
    /// first of them has room. `None` when none was.
    pub(crate) fn refusal(&self) -> Option<GatewayError> {
        self.soonest
            .as_ref()
            .map(|(label, secs)| GatewayError::rate_limited(label.clone(), *secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(rpm: Option<u32>, tpm: Option<u32>) -> RouteCaps {
        RouteCaps {
            route_id: Uuid::new_v4(),
            rpm,
            tpm,
        }
    }

    /// Redis Cluster runs a script only when all its keys share a slot:
    /// a route's two counters do, by their tag.
    #[test]
    fn a_routes_counters_share_one_slot() {
        use fred::util::redis_keyslot;
        let route = Uuid::new_v4();
        let requests = counter_key(route, RateMetric::Requests);
        let tokens = counter_key(route, RateMetric::Tokens);
        assert_eq!(
            requests,
            format!("ratelimit:{{route:{route}}}:ai_gateway:route:{route}:requests:60")
        );
        assert_eq!(
            redis_keyslot(requests.as_bytes()),
            redis_keyslot(tokens.as_bytes())
        );
        assert_eq!(
            redis_keyslot(requests.as_bytes()),
            redis_keyslot(format!("route:{route}").as_bytes())
        );
    }

    /// Only the caps a route has are counted; an admitted request adds
    /// one to the request count and nothing to the token count.
    #[test]
    fn only_set_caps_are_counted() {
        assert!(caps(None, None).counters().is_empty());

        let both = caps(Some(5), Some(1_000)).counters();
        assert_eq!(both.len(), 2);
        let (metric, requests) = &both[0];
        assert_eq!(*metric, RateMetric::Requests);
        assert_eq!((requests.max_count, requests.charge), (5, 1));
        assert_eq!(requests.bucket_secs, 1);
        let (metric, tokens) = &both[1];
        assert_eq!(*metric, RateMetric::Tokens);
        assert_eq!((tokens.max_count, tokens.charge), (1_000, 0));

        let tokens_only = caps(None, Some(10)).counters();
        assert_eq!(tokens_only.len(), 1);
        assert_eq!(tokens_only[0].0, RateMetric::Tokens);
    }

    /// A never-connected client times every command out: Redis down.
    fn unreachable_redis() -> Client {
        use fred::types::Builder;
        use fred::types::config::Config;
        Builder::from_config(Config::from_url("redis://127.0.0.1:1").unwrap())
            .with_performance_config(|c| {
                c.default_command_timeout = std::time::Duration::from_millis(50)
            })
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn an_uncapped_route_is_admitted_without_redis() {
        let admitted = admit(&unreachable_redis(), &caps(None, None), true).await;
        assert_eq!(admitted.unwrap(), Admission::Admitted);
    }

    #[tokio::test]
    async fn an_unreachable_store_follows_the_fail_closed_setting() {
        let capped = caps(Some(1), None);
        let open = admit(&unreachable_redis(), &capped, false).await;
        assert_eq!(open.unwrap(), Admission::Admitted);

        let closed = admit(&unreachable_redis(), &capped, true)
            .await
            .unwrap_err();
        assert_eq!(closed.status_code(), 429);
        assert_eq!(closed.to_string(), "Rate limited: rate_limiter_unavailable");
    }

    /// Every candidate at its cap: 429, retry when the first frees.
    #[test]
    fn the_refusal_waits_for_the_first_route_to_free() {
        let mut skipped = CappedRoutes::default();
        assert!(skipped.refusal().is_none());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        skipped.add(a, label(RateMetric::Requests), 40);
        skipped.add(b, label(RateMetric::Tokens), 12);
        assert_eq!(skipped.routes(), &[a, b]);
        let refusal = skipped.refusal().unwrap();
        assert_eq!(refusal.status_code(), 429);
        assert_eq!(refusal.retry_after_secs(), Some(12));
        assert_eq!(refusal.to_string(), "Rate limited: route:tokens/1m");
    }
}
