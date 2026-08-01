//! Fixed-window counters must never strand a key without an expiry.
//!
//! The failure this guards against locked a real operator out of the
//! console permanently: the signing-key rate limiter's counter lost its
//! TTL, so it only ever grew, and no amount of waiting could clear it.
//! Every request then failed signature verification, the app retried the
//! login, and the retry pushed the counter further past the threshold.

use think_watch_common::fixed_window;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_counter_that_lost_its_expiry_gets_one_back() {
    // Exactly the state the old create-then-INCR code could leave
    // behind: a live counter with no TTL, which nothing would ever
    // clear.
    let app = TestApp::spawn().await;
    let key = format!("fixed_window_test:{}", unique_name("stranded"));
    let _: () =
        fred::interfaces::KeysInterface::set(&app.state.redis, &key, "7", None, None, false)
            .await
            .unwrap();
    let ttl: i64 = fred::interfaces::KeysInterface::ttl(&app.state.redis, &key)
        .await
        .unwrap();
    assert_eq!(ttl, -1, "precondition: the key starts with no expiry");

    let count = fixed_window::incr(&app.state.redis, &key, 60)
        .await
        .unwrap();
    assert_eq!(count, 8, "the existing count must be preserved, not reset");

    let ttl: i64 = fred::interfaces::KeysInterface::ttl(&app.state.redis, &key)
        .await
        .unwrap();
    assert!(
        ttl > 0 && ttl <= 60,
        "a stranded counter must be given an expiry so the window can end, got ttl={ttl}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_running_window_keeps_its_original_deadline() {
    // Fixed window, not sliding: refreshing the TTL on every hit would
    // let steady traffic hold a counter open indefinitely, which is the
    // opposite of the bug above but just as wrong.
    let app = TestApp::spawn().await;
    let key = format!("fixed_window_test:{}", unique_name("running"));

    assert_eq!(
        fixed_window::incr(&app.state.redis, &key, 60)
            .await
            .unwrap(),
        1
    );
    let _: () = fred::interfaces::KeysInterface::expire(&app.state.redis, &key, 5, None)
        .await
        .unwrap();

    assert_eq!(
        fixed_window::incr(&app.state.redis, &key, 60)
            .await
            .unwrap(),
        2
    );
    let ttl: i64 = fred::interfaces::KeysInterface::ttl(&app.state.redis, &key)
        .await
        .unwrap();
    assert!(
        ttl <= 5,
        "an already-expiring window must not have its deadline pushed out, got ttl={ttl}"
    );
}
