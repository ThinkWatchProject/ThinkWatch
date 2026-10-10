// ============================================================================
// Bucketed sliding-window rate limiter
//
// A pure-sliding window over a 1-week timespan would need ~50k members
// in a single Redis ZSET (one per request) and would dominate memory
// at any meaningful traffic. We approximate with **fixed-bucket
// sliding**: each window is split into 60 buckets, the "current value"
// is the sum of the last 60 buckets. Precision is ~1.6%, more than
// enough for rate limiting.
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
// Storage: one Redis hash per counter, its fields the bucket ids
// (`now_secs / bucket_secs`), each holding what that bucket counted:
//
//   ratelimit:{user:<owner>}:<surface>:<subject_kind>:<subject_id>:<metric>:<window_secs>
//
// `<owner>` is the user the request runs as — for a key-lineage counter,
// the key's owner. The braces are a Redis Cluster hash tag: every counter
// one request touches carries the same tag, so all of them sit in one
// slot and one script can read and write them together. Each counter is
// one declared key; the script never builds a key name of its own.
//
// A request goes through two scripts:
//
//   * `admit`, before the call: refuses when any rule's window is full
//     (`current >= max_count`) and otherwise charges 1 to every
//     `requests` rule. `tokens` rules are only read — a request can't
//     know its tokens yet. All or nothing: a refused request charges no
//     counter at all. A refusal also says how long until the limiting
//     window has room again.
//   * `record`, after the call: adds the tokens it used to every
//     `tokens` rule, unconditionally. Usage that overshoots a limit is
//     still recorded — the overshoot is bounded by what was in flight
//     when the window filled, and the next request is refused.
//
// Old buckets are pruned by `admit` as it reads them; the hash itself
// expires 2 × window after its last write.
// ============================================================================

use std::collections::HashMap;
use std::sync::OnceLock;

use fred::clients::Client;
use fred::interfaces::LuaInterface;
use sha1::{Digest, Sha1};
use uuid::Uuid;

use super::{RateLimitRule, RateLimitSubject, RateMetric, Surface};

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
// The Lua scripts — see file header for the algorithm
// ----------------------------------------------------------------------------

const LUA_ADMIT: &str = r#"
-- KEYS:    one counter hash per rule
-- ARGV:    [now_ms, then (bucket_secs, max_count, charge) per rule]
-- Returns: {allowed, limiting rule (1-based, 0 when allowed),
--           ms until it has room (0 when allowed), current per rule...}

local now_ms   = tonumber(ARGV[1])
local now_secs = math.floor(now_ms / 1000)
local n        = #KEYS

local sums = {}
local limiting, wait_ms = 0, 0
for i = 1, n do
    local bucket_secs = tonumber(ARGV[2 + (i - 1) * 3])
    local max_count   = tonumber(ARGV[3 + (i - 1) * 3])
    local current     = math.floor(now_secs / bucket_secs)
    local oldest      = current - 59

    local counts, stale, sum = {}, {}, 0
    local fields = redis.call('HGETALL', KEYS[i])
    for j = 1, #fields, 2 do
        local b = tonumber(fields[j])
        if b == nil or b < oldest then
            stale[#stale + 1] = fields[j]
        elseif b <= current then
            local v = tonumber(fields[j + 1]) or 0
            counts[b] = v
            sum = sum + v
        end
    end
    if #stale > 0 then
        redis.call('HDEL', KEYS[i], unpack(stale))
    end
    sums[i] = sum

    if sum >= max_count then
        -- The window has room for one more once enough of its oldest
        -- buckets have left it. Bucket b leaves at (b + 60) * bucket_secs.
        local need, freed, b = sum - max_count + 1, 0, oldest
        while b < current do
            freed = freed + (counts[b] or 0)
            if freed >= need then break end
            b = b + 1
        end
        local w = (b + 60) * bucket_secs * 1000 - now_ms
        if w > wait_ms then
            wait_ms = w
            limiting = i
        end
    end
end

if limiting > 0 then
    local out = {0, limiting, wait_ms}
    for i = 1, n do out[#out + 1] = sums[i] end
    return out
end

for i = 1, n do
    local charge = ARGV[4 + (i - 1) * 3]
    if tonumber(charge) > 0 then
        local bucket_secs = tonumber(ARGV[2 + (i - 1) * 3])
        redis.call('HINCRBY', KEYS[i], math.floor(now_secs / bucket_secs), charge)
        redis.call('EXPIRE', KEYS[i], bucket_secs * 120)
        sums[i] = sums[i] + tonumber(charge)
    end
end
local out = {1, 0, 0}
for i = 1, n do out[#out + 1] = sums[i] end
return out
"#;

const LUA_RECORD: &str = r#"
-- KEYS:    one counter hash per rule
-- ARGV:    [now_ms, amount, then bucket_secs per rule]
local now_secs = math.floor(tonumber(ARGV[1]) / 1000)
for i = 1, #KEYS do
    local bucket_secs = tonumber(ARGV[2 + i])
    redis.call('HINCRBY', KEYS[i], math.floor(now_secs / bucket_secs), ARGV[2])
    redis.call('EXPIRE', KEYS[i], bucket_secs * 120)
end
return #KEYS
"#;

fn sha(script: &'static str, cell: &'static OnceLock<String>) -> &'static str {
    cell.get_or_init(|| {
        let mut h = Sha1::new();
        h.update(script.as_bytes());
        hex::encode(h.finalize())
    })
}

/// EVALSHA, falling back to EVAL when the node doesn't have the script
/// cached yet (a restart, a failover, a node new to the cluster).
async fn run(
    redis: &Client,
    script: &'static str,
    cell: &'static OnceLock<String>,
    keys: Vec<String>,
    args: Vec<String>,
) -> Result<Vec<i64>, fred::error::Error> {
    match redis
        .evalsha::<Vec<i64>, _, _, _>(sha(script, cell), keys.clone(), args.clone())
        .await
    {
        Ok(v) => Ok(v),
        Err(e) if e.details().starts_with("NOSCRIPT") => redis.eval(script, keys, args).await,
        Err(e) => Err(e),
    }
}

static ADMIT_SHA: OnceLock<String> = OnceLock::new();
static RECORD_SHA: OnceLock<String> = OnceLock::new();

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// Render a rule's identity as the canonical
/// `<subject_kind>:<metric>/<window>` label (e.g.
/// `api_key_lineage:tokens/1h`, `user:requests/1m`). Used by:
/// - the HTTP body for rate-limited responses,
/// - the `gateway_logs` / `mcp_logs` audit row's `limits.label`,
/// - log scrapers that group by `{subject_kind, metric, window}`.
///
/// Lives next to the engine because every surface that runs
/// [`admit`] also needs to render the label that caused a deny —
/// keeping them apart bred two identical copies (one each in the AI
/// gateway and MCP gateway). One implementation, one format.
pub fn rate_label(rule: &super::RateLimitRule) -> String {
    let window = match rule.window_secs {
        60 => "1m".to_string(),
        300 => "5m".to_string(),
        3_600 => "1h".to_string(),
        18_000 => "5h".to_string(),
        86_400 => "1d".to_string(),
        604_800 => "1w".to_string(),
        n => format!("{n}s"),
    };
    format!(
        "{}:{}/{}",
        rule.subject_kind.as_str(),
        rule.metric.as_str(),
        window
    )
}

/// The Redis Cluster hash tag every limit counter of `owner`'s requests
/// carries, so one script can touch all of them.
pub fn hash_tag(owner: Uuid) -> String {
    format!("{{user:{owner}}}")
}

/// The Redis key of one counter. `owner` is the user the requests run
/// as: the subject itself for a user rule, the key's owner for a key
/// lineage rule.
pub fn counter_key(
    owner: Uuid,
    surface: Surface,
    subject_kind: RateLimitSubject,
    subject_id: Uuid,
    metric: RateMetric,
    window_secs: i32,
) -> String {
    format!(
        "ratelimit:{}:{}:{}:{subject_id}:{}:{window_secs}",
        hash_tag(owner),
        surface.as_str(),
        subject_kind.as_str(),
        metric.as_str()
    )
}

/// One rule's counter, ready for the scripts.
#[derive(Debug, Clone)]
pub struct ResolvedRule {
    pub id: Uuid,
    pub key: String,
    pub bucket_secs: i32,
    pub max_count: i64,
}

impl ResolvedRule {
    pub fn new(rule: &RateLimitRule, owner: Uuid) -> Self {
        Self {
            id: rule.id,
            key: counter_key(
                owner,
                rule.surface,
                rule.subject_kind,
                rule.subject_id,
                rule.metric,
                rule.window_secs,
            ),
            bucket_secs: bucket_secs(rule.window_secs),
            max_count: rule.max_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub allowed: bool,
    /// Index into the rules handed to [`admit`] of the rule that
    /// refused the request — of those whose windows were full, the one
    /// that frees last. `None` when allowed.
    pub exceeded_index: Option<usize>,
    /// Seconds until that rule's window has room for the request again,
    /// rounded up. 0 when allowed.
    pub retry_after_secs: u64,
    /// Each rule's count in its window, after this request's charge
    /// when it was allowed, in input order. Empty when Redis failed
    /// open.
    pub currents: Vec<i64>,
}

impl CheckOutcome {
    fn open() -> Self {
        Self {
            allowed: true,
            exceeded_index: None,
            retry_after_secs: 0,
            currents: Vec::new(),
        }
    }
}

/// Before the call: refuse the request if any rule's window is full,
/// otherwise charge 1 to every `requests` rule. `tokens` rules are only
/// read; [`record`] charges them after the call. Atomic across every
/// rule — a refused request charges nothing.
///
/// `owner` is the user the request runs as (see [`counter_key`]).
///
/// On Redis error the policy is controlled by `fail_open`:
///
/// * `fail_open = true` (default): returns `Ok(allowed = true)` with
///   empty `currents`, bumps `gateway_rate_limiter_fail_open_total`,
///   and lets the request through. Redis should not be a single point
///   of failure for the AI control plane.
/// * `fail_open = false`: returns the underlying `fred` error so
///   callers can refuse the request. Wired up via the
///   `security.rate_limit_fail_closed` system setting.
pub async fn admit(
    redis: &Client,
    rules: &[RateLimitRule],
    owner: Uuid,
    fail_open: bool,
) -> Result<CheckOutcome, fred::error::Error> {
    admit_at(
        redis,
        rules,
        owner,
        chrono::Utc::now().timestamp_millis(),
        fail_open,
    )
    .await
}

/// [`admit`] at a given time, in Unix milliseconds — for tests.
pub async fn admit_at(
    redis: &Client,
    rules: &[RateLimitRule],
    owner: Uuid,
    now_ms: i64,
    fail_open: bool,
) -> Result<CheckOutcome, fred::error::Error> {
    if rules.is_empty() {
        return Ok(CheckOutcome::open());
    }
    let mut keys = Vec::with_capacity(rules.len());
    let mut args = Vec::with_capacity(1 + rules.len() * 3);
    args.push(now_ms.to_string());
    for rule in rules {
        let r = ResolvedRule::new(rule, owner);
        keys.push(r.key);
        args.push(r.bucket_secs.to_string());
        args.push(r.max_count.to_string());
        args.push(match rule.metric {
            RateMetric::Requests => "1".to_string(),
            RateMetric::Tokens => "0".to_string(),
        });
    }

    let reply = match run(redis, LUA_ADMIT, &ADMIT_SHA, keys, args).await {
        Ok(v) => v,
        Err(e) if fail_open => {
            tracing::warn!("rate-limit check failed: {e}; failing open");
            metrics::counter!("gateway_rate_limiter_fail_open_total").increment(1);
            return Ok(CheckOutcome::open());
        }
        Err(e) => {
            tracing::error!(
                "rate-limit check failed: {e}; failing closed per security.rate_limit_fail_closed"
            );
            metrics::counter!("gateway_rate_limiter_fail_closed_total").increment(1);
            return Err(e);
        }
    };
    Ok(parse_admit_reply(&reply))
}

/// Reply shape: `[allowed, limiting rule (1-based, 0 = none), wait_ms,
/// currents...]`.
fn parse_admit_reply(reply: &[i64]) -> CheckOutcome {
    let allowed = reply.first().copied().unwrap_or(1) == 1;
    let limiting = reply.get(1).copied().unwrap_or(0);
    let wait_ms = reply.get(2).copied().unwrap_or(0).max(0);
    CheckOutcome {
        allowed,
        exceeded_index: (!allowed && limiting >= 1).then(|| (limiting - 1) as usize),
        retry_after_secs: if allowed {
            0
        } else {
            retry_after_secs(wait_ms)
        },
        currents: reply.iter().skip(3).copied().collect(),
    }
}

/// Whole seconds a client should wait for `wait_ms` to pass: rounded up,
/// and at least one — `Retry-After: 0` reads as "now".
pub fn retry_after_secs(wait_ms: i64) -> u64 {
    (wait_ms.max(0) as u64).div_ceil(1000).max(1)
}

/// After the call: add `amount` to every rule of `metric`, whether or
/// not that takes it past its limit — the call has been made, and
/// hiding what it used would only let the next one through too.
pub async fn record(
    redis: &Client,
    rules: &[RateLimitRule],
    owner: Uuid,
    metric: RateMetric,
    amount: i64,
) -> Result<(), fred::error::Error> {
    record_at(
        redis,
        rules,
        owner,
        metric,
        amount,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
}

/// [`record`] at a given time, in Unix milliseconds — for tests.
pub async fn record_at(
    redis: &Client,
    rules: &[RateLimitRule],
    owner: Uuid,
    metric: RateMetric,
    amount: i64,
    now_ms: i64,
) -> Result<(), fred::error::Error> {
    let resolved: Vec<ResolvedRule> = rules
        .iter()
        .filter(|r| r.metric == metric)
        .map(|r| ResolvedRule::new(r, owner))
        .collect();
    if resolved.is_empty() || amount <= 0 {
        return Ok(());
    }
    let mut args = Vec::with_capacity(2 + resolved.len());
    args.push(now_ms.to_string());
    args.push(amount.to_string());
    args.extend(resolved.iter().map(|r| r.bucket_secs.to_string()));
    let keys = resolved.into_iter().map(|r| r.key).collect();
    run(redis, LUA_RECORD, &RECORD_SHA, keys, args)
        .await
        .map(|_| ())
}

/// Read-only "what's the current sum for this rule" helper. Used by
/// the console's usage views to render "X / Y used" without any side
/// effects.
///
/// Returns 0 on Redis error so the UI can fall back to "no data"
/// rather than 500. Real failures are logged.
pub async fn current_count(redis: &Client, rule: &ResolvedRule) -> i64 {
    match read_count(redis, rule, chrono::Utc::now().timestamp()).await {
        Ok(count) => count,
        Err(e) => {
            tracing::warn!(key = %rule.key, "rate-limit usage read failed: {e}");
            0
        }
    }
}

/// What the window that ends at `now_secs` holds — the sum [`admit`]
/// compares with the limit — without changing it. A Redis error is
/// returned, not read as an empty window.
pub async fn read_count(
    redis: &Client,
    rule: &ResolvedRule,
    now_secs: i64,
) -> Result<i64, fred::error::Error> {
    use fred::interfaces::HashesInterface;
    let buckets = redis.hgetall::<HashMap<String, i64>, _>(&rule.key).await?;
    Ok(window_sum(&buckets, now_secs, rule.bucket_secs))
}

/// The sum of the buckets inside the window that ends at `now_secs`.
fn window_sum(buckets: &HashMap<String, i64>, now_secs: i64, bucket_secs: i32) -> i64 {
    let bucket_secs = i64::from(bucket_secs);
    if bucket_secs <= 0 {
        return 0;
    }
    let current = now_secs.div_euclid(bucket_secs);
    let oldest = current - (BUCKETS_PER_WINDOW - 1);
    buckets
        .iter()
        .filter_map(|(b, v)| b.parse::<i64>().ok().map(|b| (b, *v)))
        .filter(|(b, _)| (oldest..=current).contains(b))
        .map(|(_, v)| v)
        .sum()
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

    fn rule(kind: RateLimitSubject, subject: Uuid, metric: RateMetric) -> RateLimitRule {
        RateLimitRule {
            id: Uuid::nil(),
            subject_kind: kind,
            subject_id: subject,
            surface: Surface::AiGateway,
            metric,
            window_secs: 60,
            max_count: 1,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        }
    }

    /// Redis Cluster runs a script only when every key it declares hashes
    /// to one slot. A request's counters — the user's and the key
    /// lineage's, requests and tokens — must all land together.
    #[test]
    fn every_counter_of_a_request_hashes_to_one_slot() {
        use fred::util::redis_keyslot;
        let user = Uuid::new_v4();
        let lineage = Uuid::new_v4();
        let keys: Vec<String> = [
            rule(RateLimitSubject::User, user, RateMetric::Requests),
            rule(RateLimitSubject::User, user, RateMetric::Tokens),
            rule(
                RateLimitSubject::ApiKeyLineage,
                lineage,
                RateMetric::Requests,
            ),
            rule(RateLimitSubject::ApiKeyLineage, lineage, RateMetric::Tokens),
        ]
        .iter()
        .map(|r| ResolvedRule::new(r, user).key)
        .collect();
        let slot = redis_keyslot(keys[0].as_bytes());
        for k in &keys {
            assert_eq!(redis_keyslot(k.as_bytes()), slot, "{k}");
        }
        // The tag is what decides: the slot is the tag's own.
        assert_eq!(slot, redis_keyslot(format!("user:{user}").as_bytes()));
    }

    #[test]
    fn a_key_lineage_counter_is_named_after_the_lineage_under_its_owners_tag() {
        let user = Uuid::new_v4();
        let lineage = Uuid::new_v4();
        let key = ResolvedRule::new(
            &rule(RateLimitSubject::ApiKeyLineage, lineage, RateMetric::Tokens),
            user,
        )
        .key;
        assert_eq!(
            key,
            format!("ratelimit:{{user:{user}}}:ai_gateway:api_key_lineage:{lineage}:tokens:60")
        );
    }

    #[test]
    fn the_window_sum_counts_the_last_sixty_buckets_only() {
        let now = 10_000;
        let b = |id: i64, v: i64| (id.to_string(), v);
        let buckets: HashMap<String, i64> = [
            b(now, 1),      // current bucket
            b(now - 59, 2), // oldest still inside
            b(now - 60, 4), // just left
            b(now + 1, 8),  // a pod whose clock runs ahead
        ]
        .into_iter()
        .chain([("garbage".to_string(), 16)])
        .collect();
        assert_eq!(window_sum(&buckets, now, 1), 3);
        // Five-second buckets: bucket id = now / 5.
        let five: HashMap<String, i64> = [b(now / 5, 7)].into_iter().collect();
        assert_eq!(window_sum(&five, now, 5), 7);
    }

    #[test]
    fn retry_after_rounds_up_and_never_says_now() {
        assert_eq!(retry_after_secs(1), 1);
        assert_eq!(retry_after_secs(1_000), 1);
        assert_eq!(retry_after_secs(1_001), 2);
        assert_eq!(retry_after_secs(59_999), 60);
        assert_eq!(retry_after_secs(0), 1);
        assert_eq!(retry_after_secs(-5), 1);
    }

    #[test]
    fn a_refusal_names_the_limiting_rule_and_its_wait() {
        let refused = parse_admit_reply(&[0, 2, 4_500, 3, 10]);
        assert!(!refused.allowed);
        assert_eq!(refused.exceeded_index, Some(1));
        assert_eq!(refused.retry_after_secs, 5);
        assert_eq!(refused.currents, vec![3, 10]);

        let allowed = parse_admit_reply(&[1, 0, 0, 4]);
        assert!(allowed.allowed);
        assert_eq!(allowed.exceeded_index, None);
        assert_eq!(allowed.retry_after_secs, 0);
        assert_eq!(allowed.currents, vec![4]);
    }
}
