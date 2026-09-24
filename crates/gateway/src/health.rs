//! Per-route health: rolling-window error rate + EWMA latency +
//! circuit breaker. Backed by Redis so all gateway replicas share the
//! same view; drives selection-time filtering.
//!
//! ### The state machine is shared, the premises are not
//!
//! The breaker's state machine is thinkwatch-core's `tw-breaker`, the one
//! the desktop gateway and this crate's MCP breaker also run. What is
//! ours is where it lives and what trips it: state shared across
//! replicas through Redis, tripped by an error rate over a window, tuned
//! by an admin, and an open route filtered out of selection. (The desktop
//! keeps it in-process, trips on consecutive failures and fails open when
//! every candidate is down — right for one user with nowhere else to go.)
//!
//! ### Storage
//!
//! Each route gets:
//!   * `route_health:{route_id}:samples` — ZSET, score = completion
//!     timestamp_ms, member = `"<seq>:<latency_ms>:<is_error>"`.
//!     The seq number disambiguates simultaneous writes within the
//!     same millisecond; the rest is parsed back at tally time.
//!   * `route_health:{route_id}:state` — the breaker, as `tw-breaker`'s
//!     JSON. Missing or unreadable means closed.
//!   * `route_health:{route_id}:counters` — Hash. Currently a single
//!     `lifetime_requests` field, `HINCRBY`-ed by 1 on every call.
//!     Counts cumulative traffic the rolling-window `total` can't
//!     express — operators tuning weights need to know whether a
//!     route has actually carried any requests at all.
//!
//! ### One round trip, two when the state changes
//!
//! Recording a completion is one Lua call: insert the sample, drop what
//! fell out of the window, tally it, read the breaker. The transition is
//! computed here, from that tally. Only when it changes the breaker is it
//! written back, by a compare-and-set: two replicas that computed from the
//! same state cannot both write, and the one that loses was looking at a
//! state that is no longer there.
//!
//! **A cooled breaker is half-open when read**, with no write needed. It
//! used to become half-open only when a request on that route completed —
//! and an open route is never picked, so it stayed open until its state
//! key expired, about four cooldowns later.
//!
//! ### Why ZSET + Lua and not per-second Hash buckets?
//!
//! Sliding-window done right needs old samples to fall out cleanly —
//! one `ZREMRANGEBYSCORE -inf <window_start>` is O(log N + M).
//! Per-second hash buckets cost O(window_secs) reads per request,
//! which is silly. ZSET sample count is soft-capped (1000 per route)
//! so memory stays bounded under unbounded RPS — at high RPS the
//! effective window shrinks, which is fine for breaker decisions.

use fred::clients::Client;
use fred::interfaces::{HashesInterface, KeysInterface, LuaInterface, SortedSetsInterface};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use think_watch_common::dynamic_config::DynamicConfig;
use tw_breaker::{Breaker, Policy, State, Tally, Trip};
use uuid::Uuid;

const SAMPLE_CAP: u32 = 1000;

/// Counter for the seq portion of sample member ids — guarantees
/// uniqueness under simultaneous writes within the same millisecond.
static SAMPLE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Insert a sample, trim the window, tally it and read the breaker.
/// Returns `(breaker_json_or_empty, total, errs, ewma_ms_x100, lifetime)`.
const LUA_RECORD: &str = r#"
local samples_key  = KEYS[1]
local state_key    = KEYS[2]
local counters_key = KEYS[3]
local now_ms       = tonumber(ARGV[1])
local window_start = tonumber(ARGV[2])
local member       = ARGV[3]
local ttl          = tonumber(ARGV[4])
local sample_cap   = tonumber(ARGV[5])

redis.call('ZREMRANGEBYSCORE', samples_key, '-inf', window_start)
local oversize = redis.call('ZCARD', samples_key) - sample_cap
if oversize > 0 then
    redis.call('ZREMRANGEBYRANK', samples_key, 0, oversize - 1)
end
redis.call('ZADD', samples_key, now_ms, member)
redis.call('EXPIRE', samples_key, ttl)

-- Cumulative, no EXPIRE: the all-time view the window can't express.
local lifetime = redis.call('HINCRBY', counters_key, 'lifetime_requests', 1)

local members = redis.call('ZRANGEBYSCORE', samples_key, window_start, '+inf')
local total = 0
local errs = 0
local ewma_num = 0.0
local alpha = 0.2
for _, m in ipairs(members) do
    local _, lat_str, err_str = m:match('^(%d+):(%-?[%d.]+):(%d)$')
    if lat_str then
        total = total + 1
        if err_str == '1' then errs = errs + 1 end
        local l = tonumber(lat_str) or 0
        if total == 1 then ewma_num = l
        else ewma_num = alpha * l + (1.0 - alpha) * ewma_num end
    end
end

local state = redis.call('GET', state_key) or ''
return { state, total, errs, math.floor(ewma_num * 100), lifetime }
"#;

/// Write the breaker back only if nobody else has since. `ARGV[1]` is the
/// value read (empty = there was none), `ARGV[2]` the new one (empty =
/// delete: a closed breaker is the default). `ARGV[4] == '1'` also wipes
/// the window — a route that just recovered starts clean, so the errors
/// that opened it cannot drag it straight back.
const LUA_CAS: &str = r#"
local state_key   = KEYS[1]
local samples_key = KEYS[2]
local current = redis.call('GET', state_key) or ''
if current ~= ARGV[1] then return 0 end
if ARGV[2] == '' then
    redis.call('DEL', state_key)
else
    redis.call('SET', state_key, ARGV[2], 'EX', tonumber(ARGV[3]))
end
if ARGV[4] == '1' then redis.call('DEL', samples_key) end
return 1
"#;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RouteHealth {
    pub state: State,
    pub total: u32,
    pub errors: u32,
    pub error_pct: f64,
    pub ewma_latency_ms: Option<f64>,
    /// Cumulative all-time request count for this route. Survives
    /// rolling-window expiry and circuit-breaker resets — operators
    /// tuning weights use this to tell apart "no traffic yet" from
    /// "quiet right now". `HINCRBY`-backed in Redis; persists across
    /// gateway restarts since the counter hash carries no TTL.
    pub lifetime_requests: u64,
}

/// Tunables loaded from system_settings on each request — cheap to
/// re-read because DynamicConfig is in-memory.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub error_pct: u32,
    pub min_samples: u32,
    pub window_secs: u32,
    pub open_secs: u32,
}

impl CircuitBreakerConfig {
    /// The breaker settings in force. The router and the route-health
    /// page both read them here, so the page shows what selection sees.
    pub async fn load(dc: &DynamicConfig) -> Self {
        Self {
            enabled: dc.cb_enabled().await,
            error_pct: dc.cb_error_pct().await,
            min_samples: dc.cb_min_samples().await,
            window_secs: dc.cb_window_secs().await,
            open_secs: dc.cb_open_secs().await,
        }
    }

    /// As a `tw-breaker` policy: an error rate over the window, one probe.
    pub fn policy(&self) -> Policy {
        Policy {
            trip: Trip::ErrorRate {
                percent: self.error_pct,
                min_samples: self.min_samples,
            },
            cooldown: Duration::from_secs(u64::from(self.open_secs)),
            probes: 1,
        }
    }

    /// How long the samples and the breaker are kept after the last write.
    fn ttl_secs(&self) -> u32 {
        (self.open_secs * 4).max(60)
    }
}

fn keys(route_id: Uuid) -> (String, String, String) {
    (
        format!("route_health:{route_id}:samples"),
        format!("route_health:{route_id}:state"),
        format!("route_health:{route_id}:counters"),
    )
}

fn parse(raw: &str) -> Breaker {
    serde_json::from_str(raw).unwrap_or_default()
}

fn health(state: State, total: u32, errors: u32, ewma: f64, lifetime: u64) -> RouteHealth {
    let error_pct = if total > 0 {
        f64::from(errors) * 100.0 / f64::from(total)
    } else {
        0.0
    };
    RouteHealth {
        state,
        total,
        errors,
        error_pct,
        ewma_latency_ms: (total > 0 && ewma > 0.0).then_some(ewma),
        lifetime_requests: lifetime,
    }
}

#[derive(Clone)]
pub struct HealthTracker {
    redis: Client,
}

impl HealthTracker {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Record a request completion and return the route's health after it.
    /// Failures are logged and degrade gracefully — health tracking isn't
    /// on the request critical path.
    pub async fn record(
        &self,
        route_id: Uuid,
        latency_ms: u32,
        is_error: bool,
        cfg: CircuitBreakerConfig,
    ) -> RouteHealth {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let window_start = now_ms - i64::from(cfg.window_secs) * 1000;
        let seq = SAMPLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let member = format!("{seq}:{latency_ms}:{}", u8::from(is_error));
        let (samples_key, state_key, counters_key) = keys(route_id);

        let result: Result<(String, i64, i64, i64, i64), _> = self
            .redis
            .eval(
                LUA_RECORD,
                vec![
                    samples_key.as_str(),
                    state_key.as_str(),
                    counters_key.as_str(),
                ],
                vec![
                    now_ms.to_string(),
                    window_start.to_string(),
                    member,
                    cfg.ttl_secs().to_string(),
                    SAMPLE_CAP.to_string(),
                ],
            )
            .await;
        let (raw, total, errs, ewma_x100, lifetime) = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("route_health record failed: {e}");
                return RouteHealth::default();
            }
        };
        let total = u32::try_from(total.max(0)).unwrap_or(u32::MAX);
        let errors = u32::try_from(errs.max(0)).unwrap_or(u32::MAX);
        let ewma = ewma_x100 as f64 / 100.0;
        let lifetime = u64::try_from(lifetime.max(0)).unwrap_or(0);

        if !cfg.enabled {
            return health(State::Closed, total, errors, ewma, lifetime);
        }
        let policy = cfg.policy();
        let mut breaker = parse(&raw);
        let before = breaker;
        let change = breaker.record(!is_error, Tally { total, errors }, &policy, now_ms);
        if breaker != before {
            let recovered = change == Some(State::Closed);
            let next = if breaker == Breaker::default() {
                String::new()
            } else {
                serde_json::to_string(&breaker).unwrap_or_default()
            };
            let written: Result<i64, _> = self
                .redis
                .eval(
                    LUA_CAS,
                    vec![state_key.as_str(), samples_key.as_str()],
                    vec![
                        raw.clone(),
                        next,
                        cfg.ttl_secs().to_string(),
                        if recovered { "1" } else { "0" }.to_string(),
                    ],
                )
                .await;
            match written {
                Ok(1) => {
                    if let Some(s) = change {
                        tracing::info!(%route_id, state = ?s, "route circuit breaker changed state");
                    }
                }
                // Another replica wrote first; its view stands.
                Ok(_) => breaker = parse(&raw),
                Err(e) => {
                    tracing::warn!("route_health state write failed: {e}");
                    breaker = parse(&raw);
                }
            }
        }
        health(
            breaker.state_at(&policy, now_ms),
            total,
            errors,
            ewma,
            lifetime,
        )
    }

    /// The breaker's state right now — one read. A cooled open breaker is
    /// half-open. Best-effort: an error reads as closed.
    pub async fn state(&self, route_id: Uuid, cfg: CircuitBreakerConfig) -> State {
        if !cfg.enabled {
            return State::Closed;
        }
        let (_, state_key, _) = keys(route_id);
        let raw: Option<String> = self.redis.get(&state_key).await.ok().flatten();
        parse(raw.as_deref().unwrap_or(""))
            .state_at(&cfg.policy(), chrono::Utc::now().timestamp_millis())
    }

    /// Read-only snapshot — for selection-time filter and UI display.
    /// Best-effort: errors return a default (closed, no data).
    pub async fn snapshot(&self, route_id: Uuid, cfg: CircuitBreakerConfig) -> RouteHealth {
        let (samples_key, _, counters_key) = keys(route_id);
        let state = self.state(route_id, cfg).await;

        // HGET → Option<String>; missing field == route hasn't seen
        // any traffic yet, which we render as 0.
        let lifetime_raw: Option<String> = self
            .redis
            .hget(&counters_key, "lifetime_requests")
            .await
            .ok()
            .flatten();
        let lifetime = lifetime_raw
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let now_ms = chrono::Utc::now().timestamp_millis();
        let window_start = (now_ms - i64::from(cfg.window_secs) * 1000) as f64;
        let members: Vec<String> = self
            .redis
            .zrangebyscore(&samples_key, window_start, f64::INFINITY, false, None)
            .await
            .unwrap_or_default();

        let mut total = 0u32;
        let mut errs = 0u32;
        let mut ewma = 0.0f64;
        let alpha = 0.2;
        for m in &members {
            let mut parts = m.splitn(3, ':');
            let _seq = parts.next();
            let lat = parts.next().and_then(|s| s.parse::<f64>().ok());
            let err = parts.next();
            if let (Some(lat), Some(err)) = (lat, err) {
                total += 1;
                if err == "1" {
                    errs += 1;
                }
                ewma = if total == 1 {
                    lat
                } else {
                    alpha * lat + (1.0 - alpha) * ewma
                };
            }
        }
        health(state, total, errs, ewma, lifetime)
    }

    /// Bulk variant for the UI — sequential per-route reads. fred's
    /// connection multiplex amortises round-trip cost; UI cadence is
    /// low-frequency so a pipeline isn't worth the complexity.
    pub async fn snapshot_many(
        &self,
        route_ids: &[Uuid],
        cfg: CircuitBreakerConfig,
    ) -> Vec<(Uuid, RouteHealth)> {
        let mut out = Vec::with_capacity(route_ids.len());
        for id in route_ids {
            out.push((*id, self.snapshot(*id, cfg).await));
        }
        out
    }

    /// Drop every Redis key associated with a route — called when the
    /// admin deletes a route from `model_routes` so the `:samples`,
    /// `:state`, and (TTL-less) `:counters` hashes don't pile up as
    /// orphan keys after route churn. Best-effort: a Redis hiccup
    /// here is not worth failing the delete over.
    pub async fn forget(&self, route_id: Uuid) {
        let samples_key = format!("route_health:{route_id}:samples");
        let state_key = format!("route_health:{route_id}:state");
        let counters_key = format!("route_health:{route_id}:counters");
        if let Err(e) = self
            .redis
            .del::<i64, _>(vec![samples_key, state_key, counters_key])
            .await
        {
            tracing::warn!(?route_id, error = %e, "failed to purge route_health keys on route delete");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            error_pct: 50,
            min_samples: 4,
            window_secs: 60,
            open_secs: 30,
        }
    }

    /// Default health is the value the wire emits when a route has
    /// never seen a request — must include a zero lifetime counter
    /// so the UI never has to handle `undefined` for that field.
    #[test]
    fn default_route_health_is_closed_with_nothing_counted() {
        let h = RouteHealth::default();
        assert_eq!(h.state, State::Closed);
        assert_eq!(h.lifetime_requests, 0);
        assert_eq!((h.total, h.errors), (0, 0));
    }

    /// The UI reads `state` as `closed` / `open` / `half_open` and
    /// `lifetime_requests` directly off the route-health endpoint.
    #[test]
    fn route_health_serializes_the_way_the_ui_reads_it() {
        let h = health(State::HalfOpen, 3, 1, 120.5, 42);
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["state"], "half_open");
        assert_eq!(json["lifetime_requests"], 42);
        assert_eq!(json["total"], 3);
        assert!((json["error_pct"].as_f64().unwrap() - 33.33).abs() < 0.01);
    }

    #[test]
    fn the_policy_is_an_error_rate_with_one_probe() {
        let p = cfg().policy();
        assert_eq!(
            p.trip,
            Trip::ErrorRate {
                percent: 50,
                min_samples: 4
            }
        );
        assert_eq!(p.cooldown, Duration::from_secs(30));
        assert_eq!(p.probes, 1);
    }

    #[test]
    fn an_unreadable_stored_breaker_is_closed() {
        // Keys written by the previous format (`open:<ms>`) read as closed
        assert_eq!(parse("open:1700000000000"), Breaker::default());
        assert_eq!(parse(""), Breaker::default());
    }

    #[test]
    fn a_stored_open_breaker_is_half_open_once_cooled() {
        let p = cfg().policy();
        let mut b = Breaker::default();
        b.record(
            false,
            Tally {
                total: 4,
                errors: 4,
            },
            &p,
            0,
        );
        let raw = serde_json::to_string(&b).unwrap();
        assert_eq!(parse(&raw).state_at(&p, 29_000), State::Open);
        assert_eq!(parse(&raw).state_at(&p, 30_000), State::HalfOpen);
    }
}
