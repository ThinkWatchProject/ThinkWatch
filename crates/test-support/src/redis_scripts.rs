//! The rate-limit scripts, run against whatever Redis a test hands in.

use uuid::Uuid;

fn rule(
    kind: think_watch_common::limits::RateLimitSubject,
    subject: Uuid,
    metric: think_watch_common::limits::RateMetric,
    window_secs: i32,
    max_count: i64,
) -> think_watch_common::limits::RateLimitRule {
    think_watch_common::limits::RateLimitRule {
        id: Uuid::nil(),
        subject_kind: kind,
        subject_id: subject,
        surface: think_watch_common::limits::Surface::AiGateway,
        metric,
        window_secs,
        max_count,
        enabled: true,
        expires_at: None,
        reason: None,
        created_by: None,
    }
}

/// The admit/record sequence both the single-node and the cluster test
/// run: windows that fill, free bucket by bucket, refuse without
/// charging, and record past a limit.
pub async fn exercise_the_limit_scripts(redis: &fred::clients::Client) {
    use fred::interfaces::HashesInterface;
    use think_watch_common::limits::{RateLimitSubject as S, RateMetric as M, sliding};

    let owner = Uuid::new_v4();
    let lineage = Uuid::new_v4();
    let rules = [
        rule(S::User, owner, M::Requests, 60, 3),
        rule(S::ApiKeyLineage, lineage, M::Tokens, 300, 100),
    ];
    // A whole minute, so every bucket boundary below is exact.
    let t0: i64 = 1_800_000_000_000;
    let admit = |ms: i64| sliding::admit_at(redis, &rules, owner, t0 + ms, false);

    for (i, at) in [0, 10_000, 20_000].into_iter().enumerate() {
        let o = admit(at).await.unwrap();
        assert!(o.allowed, "request {i}");
        assert_eq!(o.currents, vec![i as i64 + 1, 0]);
    }
    // Full. The first request's bucket leaves the window at t0 + 60 s.
    let o = admit(30_000).await.unwrap();
    assert!(!o.allowed);
    assert_eq!(o.exceeded_index, Some(0));
    assert_eq!(o.retry_after_secs, 30);
    assert_eq!(o.currents, vec![3, 0], "a refusal charges nothing");

    // Tokens are recorded in full, past the limit.
    sliding::record_at(redis, &rules, owner, M::Tokens, 150, t0 + 30_000)
        .await
        .unwrap();
    // The requests window has room again; the tokens one does not, and
    // frees when the 5-second bucket of t0 + 30 s leaves it, 300 s on.
    let o = admit(61_000).await.unwrap();
    assert!(!o.allowed);
    assert_eq!(o.exceeded_index, Some(1));
    assert_eq!(o.retry_after_secs, 330 - 61);
    assert_eq!(o.currents, vec![2, 150]);

    // Both windows have moved past everything: one request counted, and
    // the old buckets are gone from the hash.
    let o = admit(400_000).await.unwrap();
    assert!(o.allowed);
    assert_eq!(o.currents, vec![1, 0]);
    let requests = sliding::ResolvedRule::new(&rules[0], owner);
    let tokens = sliding::ResolvedRule::new(&rules[1], owner);
    assert_eq!(redis.hlen::<i64, _>(&requests.key).await.unwrap(), 1);
    assert_eq!(redis.hlen::<i64, _>(&tokens.key).await.unwrap(), 0);
}
