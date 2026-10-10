//! The rate-limit scripts, run against whatever Redis a test hands in —
//! one node in `tests/limits.rs`, a Redis Cluster in
//! `tests/redis_cluster.rs`. The route caps run on the same scripts
//! with their own keys.

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

/// A route's caps on the same scripts: the request cap fills and counts
/// nothing more once full, recorded tokens close the token cap, and a
/// route's two counters run in one script (one hash slot).
pub async fn exercise_the_route_cap_scripts(redis: &fred::clients::Client) {
    use think_watch_gateway::route_caps::{self, Admission, RouteCaps};

    let caps = RouteCaps {
        route_id: Uuid::new_v4(),
        rpm: Some(2),
        tpm: Some(100),
    };
    let t0: i64 = 1_800_000_000_000;
    let admit = |ms: i64| route_caps::admit_at(redis, &caps, true, t0 + ms);

    assert_eq!(admit(0).await.unwrap(), Admission::Admitted);
    assert_eq!(admit(10_000).await.unwrap(), Admission::Admitted);
    // Full: the first request leaves the window at t0 + 60 s.
    assert_eq!(
        admit(20_000).await.unwrap(),
        Admission::Capped {
            label: "route:requests/1m".into(),
            retry_after_secs: 40,
        }
    );
    // Room again for one request; the tokens then fill the token cap.
    assert_eq!(admit(60_000).await.unwrap(), Admission::Admitted);
    route_caps::record_tokens_at(redis, &caps, 150, t0 + 60_000).await;
    assert_eq!(
        admit(65_000).await.unwrap(),
        Admission::Capped {
            label: "route:tokens/1m".into(),
            retry_after_secs: 55,
        }
    );
    // A minute on, both windows are clear.
    assert_eq!(admit(121_000).await.unwrap(), Admission::Admitted);
}
