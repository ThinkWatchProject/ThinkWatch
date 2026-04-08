// ============================================================================
// Bucketed sliding-window rate limiter
//
// A pure-sliding window over a 1-week timespan would need ~50k members
// in a single Redis ZSET (one per request) and would dominate memory
// at any meaningful traffic. We approximate with **fixed-bucket
// sliding**: each window is split into 60 buckets, each bucket is one
// INCR counter, the "current value" is the sum of the last 60 buckets.
// Precision is ~1.6%, more than enough for rate limiting.
//
// Bucket sizing per window:
//
//   60s   →  1s    × 60   (works for the 1m / RPS feel)
//   5m    →  5s    × 60
//   1h    →  60s   × 60
//   5h    →  5m    × 60
//   1d    →  24m   × 60
//   1w    → 168m   × 60
//
// All N rules for a request go through ONE Lua script invocation. The
// script:
//
//   1. For each rule, computes its `current` = sum(last 60 bucket counters).
//   2. Checks `current + cost <= max_count` for every rule.
//   3. If any rule would be exceeded → returns `{0, exceeded_rule_index}`
//      and writes nothing (atomic all-or-nothing).
//   4. Otherwise INCRs every rule's current bucket by `cost` and refreshes
//      the bucket TTL to `2 * window_secs` (so old buckets self-evict).
//   5. Returns `{1, ...}` on success.
//
// One round trip, one atomic decision. The script is loaded with
// SCRIPT LOAD on first use and reused via EVALSHA — see `script_sha`.
// ============================================================================

use std::sync::OnceLock;

use fred::clients::Client;
use fred::interfaces::LuaInterface;
use sha1::{Digest, Sha1};

use super::{RateLimitRule, RateMetric};

/// 60 buckets per window — fixed across every supported window size.
/// Larger N tightens precision but balloons Redis memory linearly.
pub const BUCKETS_PER_WINDOW: i64 = 60;

/// Compute the bucket size for a given window. The 60-bucket choice
/// gives clean integer divisions for all `ALLOWED_WINDOW_SECS` entries
/// (60 / 300 / 3600 / 18000 / 86400 / 604800 are all multiples of 60),
/// so this is a normal divide.
pub fn bucket_secs(window_secs: i32) -> i32 {
    window_secs / (BUCKETS_PER_WINDOW as i32)
}

// ----------------------------------------------------------------------------
// The Lua script — see file header for the algorithm
// ----------------------------------------------------------------------------

const LUA_CHECK_AND_RECORD: &str = r#"
-- KEYS:   one base key per rule (the prefix; bucket suffix is appended in-script)
-- ARGV:   [now_ms, cost, n_rules, b1_secs, m1_max, b2_secs, m2_max, ...]
-- Returns: {ok, idx_of_first_exceeded_rule_or_-1, [debug_current_per_rule...]}

local now_ms      = tonumber(ARGV[1])
local cost        = tonumber(ARGV[2])
local n_rules     = tonumber(ARGV[3])
local now_secs    = math.floor(now_ms / 1000)

-- Phase 1: read every rule's current sum, decide pass/fail.
local currents = {}
for i = 1, n_rules do
    local bucket_secs = tonumber(ARGV[3 + (i - 1) * 2 + 1])
    local max_count   = tonumber(ARGV[3 + (i - 1) * 2 + 2])
    local base_key    = KEYS[i]

    -- Window starts `bucket_secs * 60` ago. Sum the 60 buckets in
    -- [now - 60*bucket_secs, now], one MGET-style loop.
    local sum = 0
    local current_bucket = math.floor(now_secs / bucket_secs)
    for b = 0, 59 do
        local bucket_id = current_bucket - b
        local v = redis.call("GET", base_key .. ":" .. bucket_id)
        if v then sum = sum + tonumber(v) end
    end
    currents[i] = sum

    if sum + cost > max_count then
        return {0, i, currents}
    end
end

-- Phase 2: every rule passed → INCR each rule's current bucket.
for i = 1, n_rules do
    local bucket_secs = tonumber(ARGV[3 + (i - 1) * 2 + 1])
    local base_key    = KEYS[i]
    local current_bucket = math.floor(now_secs / bucket_secs)
    local k = base_key .. ":" .. current_bucket
    redis.call("INCRBY", k, cost)
    -- TTL = 2 × window so old buckets vanish before they're reused.
    redis.call("EXPIRE", k, bucket_secs * 60 * 2)
end

return {1, -1, currents}
"#;

fn script_sha() -> &'static String {
    static SHA: OnceLock<String> = OnceLock::new();
    SHA.get_or_init(|| {
        let mut h = Sha1::new();
        h.update(LUA_CHECK_AND_RECORD.as_bytes());
        hex::encode(h.finalize())
    })
}

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// One rule resolved against the current request's metric, ready to
/// hand to `check_and_record`. The `cost` parameter on the helper
/// applies to all rules in the slice equally; callers MUST pre-filter
/// the slice to a single metric before calling.
#[derive(Debug, Clone)]
pub struct ResolvedRule {
    pub id: uuid::Uuid,
    pub base_key: String,
    pub bucket_secs: i32,
    pub max_count: i64,
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub allowed: bool,
    /// Index into the `rules` slice of the first rule that would be
    /// exceeded. -1 when allowed.
    pub exceeded_index: i32,
    /// Current sum (pre-INCR) for every rule in the slice, in input
    /// order. Useful for surfacing "X / Y used" in the response or
    /// for `gateway_rate_limit_remaining` headers.
    pub currents: Vec<i64>,
}

/// Build a Redis key prefix for a rule. The bucket id is appended by
/// the Lua script. Format:
///
///   ratelimit:<surface>:<subject_kind>:<subject_id>:<metric>:<window_secs>
///
/// Splitting on the metric AND window means a (subject, surface) pair
/// can hold multiple rules without colliding their counters.
pub fn build_base_key(
    surface: &str,
    subject_kind: &str,
    subject_id: uuid::Uuid,
    metric: RateMetric,
    window_secs: i32,
) -> String {
    format!(
        "ratelimit:{surface}:{subject_kind}:{subject_id}:{}:{window_secs}",
        metric.as_str()
    )
}

/// Convert a slice of `RateLimitRule` rows into the engine-facing
/// `ResolvedRule` shape, dropping any rules whose metric doesn't match
/// `metric_filter`. Use this to split a "load every rule" result into
/// the requests-pass and tokens-pass batches.
pub fn resolve_rules(rules: &[RateLimitRule], metric_filter: RateMetric) -> Vec<ResolvedRule> {
    rules
        .iter()
        .filter(|r| r.metric == metric_filter)
        .map(|r| ResolvedRule {
            id: r.id,
            base_key: build_base_key(
                r.surface.as_str(),
                r.subject_kind.as_str(),
                r.subject_id,
                r.metric,
                r.window_secs,
            ),
            bucket_secs: bucket_secs(r.window_secs),
            max_count: r.max_count,
        })
        .collect()
}

/// Atomic "check then INCR-by-cost" for an entire batch of rules.
///
/// `cost` is in metric units: `1` for `requests`, weighted-token count
/// for `tokens`. All rules in the slice MUST share a single metric and
/// must already be filtered through `resolve_rules`.
///
/// On Redis error this returns `Ok(allowed = true)` with empty
/// `currents` so the gateway fails open. The caller bumps a metric
/// when this happens. Switching to fail-closed is a one-line change
/// behind the `security.rate_limit_fail_closed` system setting in a
/// later phase.
pub async fn check_and_record(
    redis: &Client,
    rules: &[ResolvedRule],
    cost: i64,
) -> Result<CheckOutcome, fred::error::Error> {
    if rules.is_empty() || cost <= 0 {
        return Ok(CheckOutcome {
            allowed: true,
            exceeded_index: -1,
            currents: Vec::new(),
        });
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let n_rules = rules.len();

    // Build KEYS[] in the same order as the loop below appends ARGV
    // pairs so the script's index math lines up.
    let keys: Vec<String> = rules.iter().map(|r| r.base_key.clone()).collect();

    // ARGV layout: [now_ms, cost, n_rules, then (bucket_secs, max_count) × n]
    let mut args: Vec<String> = Vec::with_capacity(3 + n_rules * 2);
    args.push(now_ms.to_string());
    args.push(cost.to_string());
    args.push(n_rules.to_string());
    for r in rules {
        args.push(r.bucket_secs.to_string());
        args.push(r.max_count.to_string());
    }

    // Try EVALSHA first; on NOSCRIPT load and retry. fred's evalsha
    // surfaces script-not-found as an error so we fall back manually.
    let raw: Result<Vec<i64>, _> = redis
        .evalsha(script_sha().as_str(), keys.clone(), args.clone())
        .await;
    let result: Vec<i64> = match raw {
        Ok(v) => v,
        Err(e) => {
            // Either NOSCRIPT or a transient redis problem. Try a
            // direct EVAL once — Redis caches the loaded script for
            // subsequent EVALSHA calls.
            tracing::debug!("rate-limit evalsha failed ({e}); falling back to EVAL");
            match redis
                .eval::<Vec<i64>, _, _, _>(LUA_CHECK_AND_RECORD, keys, args)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("rate-limit EVAL failed: {e}; failing open");
                    metrics::counter!("gateway_rate_limiter_fail_open_total").increment(1);
                    return Ok(CheckOutcome {
                        allowed: true,
                        exceeded_index: -1,
                        currents: Vec::new(),
                    });
                }
            }
        }
    };

    // Lua return shape: [ok, exceeded_index, currents...]
    let ok = result.first().copied().unwrap_or(1);
    let exceeded_index = result.get(1).copied().unwrap_or(-1) as i32;
    let currents = result.iter().skip(2).copied().collect();

    Ok(CheckOutcome {
        allowed: ok == 1,
        // Lua is 1-indexed; convert to 0-indexed for Rust callers.
        exceeded_index: if exceeded_index >= 1 {
            exceeded_index - 1
        } else {
            -1
        },
        currents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_secs_divides_evenly() {
        for &w in super::super::ALLOWED_WINDOW_SECS {
            assert_eq!(w % BUCKETS_PER_WINDOW as i32, 0, "window {w} must divide");
            assert!(bucket_secs(w) >= 1, "window {w} bucket too small");
        }
    }

    #[test]
    fn bucket_secs_examples() {
        assert_eq!(bucket_secs(60), 1);
        assert_eq!(bucket_secs(300), 5);
        assert_eq!(bucket_secs(3600), 60);
        assert_eq!(bucket_secs(18_000), 300);
        assert_eq!(bucket_secs(86_400), 1_440);
        assert_eq!(bucket_secs(604_800), 10_080);
    }
}
