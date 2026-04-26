//! Per-route health: rolling-window error rate + EWMA latency +
//! circuit-breaker state machine. Backed by Redis so all gateway
//! replicas share the same view (the project-existing `failover.rs`
//! breaker is per-process-mutex; this module is the multi-instance
//! companion that drives selection-time filtering).
//!
//! ### Storage
//!
//! Each route gets:
//!   * `route_health:{route_id}:samples` — ZSET, score = completion
//!     timestamp_ms, member = `"<seq>:<latency_ms>:<is_error>"`.
//!     The seq number disambiguates simultaneous writes within the
//!     same millisecond; the rest is parsed back at tally time.
//!   * `route_health:{route_id}:state` — small string `"<state>:<opened_at_ms>"`.
//!     Empty / missing means "closed" (optimistic default). State
//!     transitions are written atomically inside the Lua script
//!     alongside the sample insert.
//!
//! ### Circuit-breaker semantics
//!
//! - **Closed**: pass-through. After every record, if rolling-window
//!   sample count ≥ `min_samples` AND error_pct ≥ `error_pct`, trip
//!   to Open and stamp `opened_at_ms = now`.
//! - **Open**: filtered out at selection time. After
//!   `now - opened_at_ms ≥ open_secs`, transition to HalfOpen.
//! - **HalfOpen**: selectable again. The next completion decides:
//!   success ⇒ Closed (and reset window so old errors don't drag
//!   us back); error ⇒ Open again (re-stamp opened_at_ms).
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
use fred::interfaces::{KeysInterface, LuaInterface, SortedSetsInterface};
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

const SAMPLE_CAP: u32 = 1000;

/// Counter for the seq portion of sample member ids — guarantees
/// uniqueness under simultaneous writes within the same millisecond.
static SAMPLE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomic record + breaker transition. Returns the post-update
/// `(state, total, errs, ewma_ms_x100)` so the caller can include
/// these in the decision log without a second round-trip.
const LUA_RECORD: &str = r#"
local samples_key  = KEYS[1]
local state_key    = KEYS[2]
local now_ms       = tonumber(ARGV[1])
local window_start = tonumber(ARGV[2])
local member       = ARGV[3]
local latency_ms   = tonumber(ARGV[4])
local is_error     = tonumber(ARGV[5])
local cb_enabled   = tonumber(ARGV[6])
local error_pct    = tonumber(ARGV[7])
local min_samples  = tonumber(ARGV[8])
local open_secs    = tonumber(ARGV[9])
local sample_cap   = tonumber(ARGV[10])

-- Drop expired samples + over-cap entries.
redis.call('ZREMRANGEBYSCORE', samples_key, '-inf', window_start)
local oversize = redis.call('ZCARD', samples_key) - sample_cap
if oversize > 0 then
    redis.call('ZREMRANGEBYRANK', samples_key, 0, oversize - 1)
end

-- Insert new sample.
redis.call('ZADD', samples_key, now_ms, member)
redis.call('EXPIRE', samples_key, math.max(60, open_secs * 4))

-- Tally rolling window from member names: format "<seq>:<lat>:<err>".
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

-- Read current breaker state.
local raw = redis.call('GET', state_key)
local state = 'closed'
local opened_at = 0
if raw then
    local s, t = raw:match('^([a-z_]+):(%d+)$')
    if s then state = s; opened_at = tonumber(t) or 0 end
end

local new_state = state

if cb_enabled == 1 then
    if state == 'open' then
        if now_ms - opened_at >= open_secs * 1000 then
            new_state = 'half_open'
            redis.call('SET', state_key, 'half_open:' .. now_ms,
                'EX', math.max(60, open_secs * 4))
        end
    elseif state == 'half_open' then
        if is_error == 1 then
            new_state = 'open'
            redis.call('SET', state_key, 'open:' .. now_ms,
                'EX', math.max(60, open_secs * 4))
        else
            new_state = 'closed'
            redis.call('DEL', state_key)
            -- Wipe samples so old errors don't drag us back open.
            redis.call('DEL', samples_key)
            total = 0; errs = 0; ewma_num = 0
        end
    else  -- closed
        if total >= min_samples and (errs * 100) >= (error_pct * total) then
            new_state = 'open'
            redis.call('SET', state_key, 'open:' .. now_ms,
                'EX', math.max(60, open_secs * 4))
        end
    end
end

-- Re-insert this sample if it got wiped on half_open success.
if new_state == 'closed' and state == 'half_open' and is_error == 0 then
    redis.call('ZADD', samples_key, now_ms, member)
    redis.call('EXPIRE', samples_key, math.max(60, open_secs * 4))
    total = 1
    ewma_num = latency_ms
end

return { new_state, total, errs, math.floor(ewma_num * 100) }
"#;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RouteHealth {
    pub state: BreakerState,
    pub total: u32,
    pub errors: u32,
    pub error_pct: f64,
    pub ewma_latency_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    #[default]
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
    pub fn from_redis(s: &str) -> Self {
        match s {
            "open" => Self::Open,
            "half_open" => Self::HalfOpen,
            _ => Self::Closed,
        }
    }
    /// Open routes are excluded; half_open lets a probe through. We
    /// don't gate concurrency on the probe — any race resolves by
    /// the first completion picking the next state, good enough as
    /// an autotune signal.
    pub fn allows_selection(self) -> bool {
        !matches!(self, Self::Open)
    }
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

#[derive(Clone)]
pub struct HealthTracker {
    redis: Client,
}

impl HealthTracker {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Record a request completion. Atomic via Lua: writes the
    /// sample, recomputes the rolling tally, transitions the breaker,
    /// returns the post-update health snapshot. Failures are logged
    /// and degrade gracefully — health tracking isn't on the request
    /// critical path.
    pub async fn record(
        &self,
        route_id: Uuid,
        latency_ms: u32,
        is_error: bool,
        cfg: CircuitBreakerConfig,
    ) -> RouteHealth {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let window_start = now_ms - (cfg.window_secs as i64) * 1000;
        let seq = SAMPLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let member = format!("{seq}:{latency_ms}:{}", if is_error { 1 } else { 0 });
        let samples_key = format!("route_health:{route_id}:samples");
        let state_key = format!("route_health:{route_id}:state");

        // Lua returns [state_string, total, errors, ewma_ms_x100].
        // Decoded as a tuple of fred-supported scalar types — Vec of
        // mixed-type Lua replies isn't directly FromValue-compatible.
        let result: Result<(String, i64, i64, i64), _> = self
            .redis
            .eval(
                LUA_RECORD,
                vec![samples_key.as_str(), state_key.as_str()],
                vec![
                    now_ms.to_string(),
                    window_start.to_string(),
                    member,
                    latency_ms.to_string(),
                    (if is_error { 1 } else { 0 }).to_string(),
                    (if cfg.enabled { 1 } else { 0 }).to_string(),
                    cfg.error_pct.to_string(),
                    cfg.min_samples.to_string(),
                    cfg.open_secs.to_string(),
                    SAMPLE_CAP.to_string(),
                ],
            )
            .await;

        match result {
            Ok((state, total, errs, ewma_x100)) => {
                let total_u = total.max(0) as u32;
                let errs_u = errs.max(0) as u32;
                let error_pct = if total_u > 0 {
                    errs_u as f64 * 100.0 / total_u as f64
                } else {
                    0.0
                };
                let ewma = (ewma_x100 as f64) / 100.0;
                RouteHealth {
                    state: BreakerState::from_redis(&state),
                    total: total_u,
                    errors: errs_u,
                    error_pct,
                    ewma_latency_ms: if ewma > 0.0 { Some(ewma) } else { None },
                }
            }
            Err(e) => {
                tracing::warn!("route_health record failed: {e}");
                RouteHealth::default()
            }
        }
    }

    /// Read-only snapshot — for selection-time filter and UI display.
    /// No state mutation, so done in Rust rather than Lua.
    /// Best-effort: errors return a default (closed, no data).
    pub async fn snapshot(&self, route_id: Uuid, window_secs: u32) -> RouteHealth {
        let samples_key = format!("route_health:{route_id}:samples");
        let state_key = format!("route_health:{route_id}:state");

        let state_raw: Option<String> = self.redis.get(&state_key).await.ok().flatten();
        let state = state_raw
            .as_deref()
            .and_then(|s| s.split(':').next())
            .map(BreakerState::from_redis)
            .unwrap_or_default();

        let now_ms = chrono::Utc::now().timestamp_millis();
        let window_start = (now_ms - (window_secs as i64) * 1000) as f64;
        let plus_inf = f64::INFINITY;
        let members: Vec<String> = self
            .redis
            .zrangebyscore(&samples_key, window_start, plus_inf, false, None)
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

        let error_pct = if total > 0 {
            errs as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        RouteHealth {
            state,
            total,
            errors: errs,
            error_pct,
            ewma_latency_ms: if total > 0 { Some(ewma) } else { None },
        }
    }

    /// Bulk variant for the UI — sequential per-route reads. fred's
    /// connection multiplex amortises round-trip cost; UI cadence is
    /// low-frequency so a pipeline isn't worth the complexity.
    pub async fn snapshot_many(
        &self,
        route_ids: &[Uuid],
        window_secs: u32,
    ) -> Vec<(Uuid, RouteHealth)> {
        let mut out = Vec::with_capacity(route_ids.len());
        for id in route_ids {
            out.push((*id, self.snapshot(*id, window_secs).await));
        }
        out
    }
}
