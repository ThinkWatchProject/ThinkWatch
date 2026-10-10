// ============================================================================
// Per-key calendar usage
//
// What each API key (lineage) has been let through and used today and
// this month, UTC, whether or not it has limits of its own: the rate-limit
// and budget counters only exist for the limits that are configured, so
// they can't say what a key without limits has done. `GET /v1/usage`
// reads these.
//
// They count what the limits count, at the moments the limits engine
// counts it: a request once it has passed the rate limits (where
// `sliding::admit` charges `requests` rules), and its weighted tokens
// after the call (where `sliding::record` and
// `budget::add_weighted_tokens` add them). A request the limits refuse
// counts nothing.
//
// Storage (Redis): one hash per lineage per period, fields `requests`
// and `tokens`:
//
//   key_usage:{api_key_lineage:<lineage_id>}:daily:2026-10-11
//   key_usage:{api_key_lineage:<lineage_id>}:monthly:2026-10
//
// The braces are a Redis Cluster hash tag, so one script writes both.
// Each hash expires 2 × its period after its last write, as the budget
// counters do.
// ============================================================================

use chrono::{DateTime, Utc};
use fred::clients::Client;
use fred::interfaces::{HashesInterface, LuaInterface};
use uuid::Uuid;

use super::budget::bucket_id;

const DAY_TTL_SECS: i64 = 2 * 86_400;
const MONTH_TTL_SECS: i64 = 2 * 31 * 86_400;

/// What one key has used today and this month (UTC).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyUsageCounts {
    pub requests_today: i64,
    pub tokens_today: i64,
    pub requests_month: i64,
    pub tokens_month: i64,
}

/// The two hashes of `lineage` for the day and the month containing `now`.
pub fn keys(lineage: Uuid, now: DateTime<Utc>) -> [String; 2] {
    ["daily", "monthly"].map(|period| {
        format!(
            "key_usage:{{api_key_lineage:{lineage}}}:{period}:{}",
            bucket_id(period, now)
        )
    })
}

const LUA_ADD: &str = r#"
-- KEYS: day hash, month hash
-- ARGV: field, amount, day ttl, month ttl
redis.call('HINCRBY', KEYS[1], ARGV[1], ARGV[2])
redis.call('EXPIRE', KEYS[1], ARGV[3])
redis.call('HINCRBY', KEYS[2], ARGV[1], ARGV[2])
redis.call('EXPIRE', KEYS[2], ARGV[4])
return 1
"#;

async fn add(
    redis: &Client,
    lineage: Uuid,
    field: &str,
    amount: i64,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    if amount <= 0 {
        return Ok(());
    }
    let _: i64 = redis
        .eval(
            LUA_ADD,
            keys(lineage, now).to_vec(),
            vec![
                field.to_string(),
                amount.to_string(),
                DAY_TTL_SECS.to_string(),
                MONTH_TTL_SECS.to_string(),
            ],
        )
        .await?;
    Ok(())
}

/// Count one request the rate limits let through.
pub async fn record_request(
    redis: &Client,
    lineage: Uuid,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    add(redis, lineage, "requests", 1, now).await
}

/// Add a call's weighted tokens — the amount its limits were charged.
pub async fn record_tokens(
    redis: &Client,
    lineage: Uuid,
    weighted_tokens: i64,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    add(redis, lineage, "tokens", weighted_tokens, now).await
}

/// Read `lineage`'s counts for the day and month containing `now`. A
/// period nothing was counted in reads as zero; a Redis error is
/// returned.
pub async fn read(
    redis: &Client,
    lineage: Uuid,
    now: DateTime<Utc>,
) -> Result<KeyUsageCounts, fred::error::Error> {
    let [day, month] = keys(lineage, now);
    let fields = || vec!["requests", "tokens"];
    let d: Vec<Option<i64>> = redis.hmget(&day, fields()).await?;
    let m: Vec<Option<i64>> = redis.hmget(&month, fields()).await?;
    let at = |v: &[Option<i64>], i: usize| v.get(i).copied().flatten().unwrap_or(0);
    Ok(KeyUsageCounts {
        requests_today: at(&d, 0),
        tokens_today: at(&d, 1),
        requests_month: at(&m, 0),
        tokens_month: at(&m, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn a_keys_day_and_month_share_one_cluster_slot() {
        use fred::util::redis_keyslot;
        let lineage = Uuid::new_v4();
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 23, 59, 59).unwrap();
        let [day, month] = keys(lineage, now);
        assert_eq!(
            day,
            format!("key_usage:{{api_key_lineage:{lineage}}}:daily:2026-10-11")
        );
        assert_eq!(
            month,
            format!("key_usage:{{api_key_lineage:{lineage}}}:monthly:2026-10")
        );
        assert_eq!(
            redis_keyslot(day.as_bytes()),
            redis_keyslot(month.as_bytes())
        );
    }

    #[test]
    fn the_buckets_turn_over_at_utc_midnight_and_the_first() {
        let lineage = Uuid::nil();
        let before = Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 59).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let [d1, m1] = keys(lineage, before);
        let [d2, m2] = keys(lineage, after);
        assert_ne!(d1, d2);
        assert_ne!(m1, m2);
        assert!(d2.ends_with(":daily:2026-11-01") && m2.ends_with(":monthly:2026-11"));
    }
}
