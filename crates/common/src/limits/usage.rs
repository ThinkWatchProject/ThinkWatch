// ============================================================================
// Calendar usage per API key and per user
//
// What each API key (lineage) and each user has been let through and used
// today and this month, UTC, whether or not they have limits: the
// rate-limit and budget counters only exist for the limits that are
// configured, and only from when they were, so they can't say what a key
// or a user without limits has done. `GET /v1/usage` reads these.
//
// A request made with a key counts on the key and on the key's owner, so
// a user's count is the sum over all their keys. They count what the
// limits count, at the moments the limits engine counts it: a request
// once it has passed the rate limits (where `sliding::admit` charges
// `requests` rules), and its weighted tokens after the call (where
// `sliding::record` and `budget::add_weighted_tokens` add them) or when
// it is answered from the response cache. A request the limits refuse
// counts nothing.
//
// Storage (Redis): one hash per subject per period, fields `requests`
// and `tokens`:
//
//   usage:{user:<owner>}:user:<owner>:daily:2026-10-11
//   usage:{user:<owner>}:user:<owner>:monthly:2026-10
//   usage:{user:<owner>}:api_key_lineage:<lineage_id>:daily:2026-10-11
//   usage:{user:<owner>}:api_key_lineage:<lineage_id>:monthly:2026-10
//
// `<owner>` is the user the request runs as — for a key, its owner. The
// braces are a Redis Cluster hash tag, as on the rate-limit counters, so
// one script writes all four. Each hash expires 2 × its period after its
// last write, as the budget counters do.
// ============================================================================

use chrono::{DateTime, Utc};
use fred::clients::Client;
use fred::interfaces::{HashesInterface, LuaInterface};
use uuid::Uuid;

use super::budget::bucket_id;

const DAY_TTL_SECS: i64 = 2 * 86_400;
const MONTH_TTL_SECS: i64 = 2 * 31 * 86_400;

/// What one key or one user has used today and this month (UTC).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub requests_today: i64,
    pub tokens_today: i64,
    pub requests_month: i64,
    pub tokens_month: i64,
}

/// Whose counts: a user's, over all their keys, or one key's (its
/// lineage, across rotations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSubject {
    User,
    ApiKeyLineage(Uuid),
}

/// The day and month hashes of `subject`, a key of `owner` or `owner`
/// itself, for the day and month containing `now`.
pub fn keys(owner: Uuid, subject: UsageSubject, now: DateTime<Utc>) -> [String; 2] {
    let (kind, id) = match subject {
        UsageSubject::User => ("user", owner),
        UsageSubject::ApiKeyLineage(lineage) => ("api_key_lineage", lineage),
    };
    ["daily", "monthly"].map(|period| {
        format!(
            "usage:{{user:{owner}}}:{kind}:{id}:{period}:{}",
            bucket_id(period, now)
        )
    })
}

const LUA_ADD: &str = r#"
-- KEYS: day hash, month hash, per subject
-- ARGV: field, amount, day ttl, month ttl
for i = 1, #KEYS do
    redis.call('HINCRBY', KEYS[i], ARGV[1], ARGV[2])
    if i % 2 == 1 then
        redis.call('EXPIRE', KEYS[i], ARGV[3])
    else
        redis.call('EXPIRE', KEYS[i], ARGV[4])
    end
end
return 1
"#;

/// Add `amount` to `field` on the key's and its owner's day and month.
async fn add(
    redis: &Client,
    owner: Uuid,
    lineage: Uuid,
    field: &str,
    amount: i64,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    if amount <= 0 {
        return Ok(());
    }
    let mut hashes = keys(owner, UsageSubject::ApiKeyLineage(lineage), now).to_vec();
    hashes.extend(keys(owner, UsageSubject::User, now));
    let _: i64 = redis
        .eval(
            LUA_ADD,
            hashes,
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

/// Count one request the rate limits let through, made by `owner` with
/// the key of `lineage`.
pub async fn record_request(
    redis: &Client,
    owner: Uuid,
    lineage: Uuid,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    add(redis, owner, lineage, "requests", 1, now).await
}

/// Add a call's weighted tokens — the amount its limits are charged —
/// on the key of `lineage` and its owner.
pub async fn record_tokens(
    redis: &Client,
    owner: Uuid,
    lineage: Uuid,
    weighted_tokens: i64,
    now: DateTime<Utc>,
) -> Result<(), fred::error::Error> {
    add(redis, owner, lineage, "tokens", weighted_tokens, now).await
}

/// Read `subject`'s counts for the day and month containing `now`. A
/// period nothing was counted in reads as zero; a Redis error is
/// returned.
pub async fn read(
    redis: &Client,
    owner: Uuid,
    subject: UsageSubject,
    now: DateTime<Utc>,
) -> Result<UsageCounts, fred::error::Error> {
    let [day, month] = keys(owner, subject, now);
    let fields = || vec!["requests", "tokens"];
    let d: Vec<Option<i64>> = redis.hmget(&day, fields()).await?;
    let m: Vec<Option<i64>> = redis.hmget(&month, fields()).await?;
    let at = |v: &[Option<i64>], i: usize| v.get(i).copied().flatten().unwrap_or(0);
    Ok(UsageCounts {
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
    fn a_key_and_its_owner_share_one_cluster_slot() {
        use fred::util::redis_keyslot;
        let owner = Uuid::new_v4();
        let lineage = Uuid::new_v4();
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 23, 59, 59).unwrap();
        let [key_day, key_month] = keys(owner, UsageSubject::ApiKeyLineage(lineage), now);
        let [user_day, user_month] = keys(owner, UsageSubject::User, now);
        assert_eq!(
            key_day,
            format!("usage:{{user:{owner}}}:api_key_lineage:{lineage}:daily:2026-10-11")
        );
        assert_eq!(
            key_month,
            format!("usage:{{user:{owner}}}:api_key_lineage:{lineage}:monthly:2026-10")
        );
        assert_eq!(
            user_day,
            format!("usage:{{user:{owner}}}:user:{owner}:daily:2026-10-11")
        );
        assert_eq!(
            user_month,
            format!("usage:{{user:{owner}}}:user:{owner}:monthly:2026-10")
        );
        let slot = redis_keyslot(key_day.as_bytes());
        for k in [&key_month, &user_day, &user_month] {
            assert_eq!(redis_keyslot(k.as_bytes()), slot, "{k}");
        }
    }

    #[test]
    fn the_buckets_turn_over_at_utc_midnight_and_the_first() {
        let owner = Uuid::nil();
        let before = Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 59).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let [d1, m1] = keys(owner, UsageSubject::User, before);
        let [d2, m2] = keys(owner, UsageSubject::User, after);
        assert_ne!(d1, d2);
        assert_ne!(m1, m2);
        assert!(d2.ends_with(":daily:2026-11-01") && m2.ends_with(":monthly:2026-11"));
    }
}
