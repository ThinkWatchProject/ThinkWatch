//! The server against a Redis that speaks TLS only (`rediss://`).
//!
//! Managed Redis services usually require TLS. These tests point at a
//! TLS-only Redis given by `TEST_REDIS_TLS_URL` (e.g.
//! `rediss://:pw@localhost:46380`) whose certificate the CA in
//! `TEST_REDIS_CA_CERT` signed, and skip without the URL, so CI — which
//! has a plain Redis — stays green. To run them locally, make a throwaway
//! CA and a certificate for `localhost` and `127.0.0.1`, and start Redis
//! with TLS on and its plain port off:
//!
//! ```text
//! mkdir -p /tmp/tw-tls && cd /tmp/tw-tls
//! openssl req -x509 -new -nodes -newkey rsa:2048 -keyout ca.key -out ca.crt \
//!   -days 2 -subj "/CN=ThinkWatch test CA"
//! openssl req -new -nodes -newkey rsa:2048 -keyout server.key -out server.csr \
//!   -subj "/CN=localhost"
//! printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' > san.cnf
//! openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
//!   -days 2 -extfile san.cnf -out server.crt
//! chmod 644 server.key   # readable by the container's redis user
//! docker run -d --rm --name tw-redis-tls -p 46380:6380 -v /tmp/tw-tls:/tls:ro \
//!   redis:8-alpine redis-server --port 0 --tls-port 6380 \
//!   --tls-cert-file /tls/server.crt --tls-key-file /tls/server.key \
//!   --tls-ca-cert-file /tls/ca.crt --tls-auth-clients no --requirepass pw
//! TEST_REDIS_TLS_URL=rediss://:pw@localhost:46380 TEST_REDIS_CA_CERT=/tmp/tw-tls/ca.crt \
//!   cargo nextest run -p think-watch-test-support --test redis_tls --run-ignored only
//! ```
//!
//! The whole suite runs over TLS the same way: `TEST_REDIS_URL` set to
//! `rediss://:pw@localhost:46380/1` with `TEST_REDIS_CA_CERT`.
//!
//! A TLS Redis Cluster (`rediss-cluster://`) runs `tests/redis_cluster.rs`.
//! Its nodes announce `127.0.0.1`, which the certificate above names. The
//! cluster bus authenticates both of its ends with that certificate, so it
//! must not be limited to server use (the commands above set no
//! `extendedKeyUsage`):
//!
//! ```text
//! docker run -d --rm --name tw-redis-cluster-tls -v /tmp/tw-tls:/tls:ro \
//!   -p 37101:37101 -p 37102:37102 -p 37103:37103 redis:8-alpine sh -c '
//!   tls="--tls-cert-file /tls/server.crt --tls-key-file /tls/server.key
//!        --tls-ca-cert-file /tls/ca.crt --tls-auth-clients no"
//!   for p in 37101 37102 37103; do
//!     redis-server --port 0 --tls-port $p --tls-cluster yes $tls \
//!       --cluster-enabled yes --cluster-config-file n-$p.conf \
//!       --cluster-announce-ip 127.0.0.1 --protected-mode no --save "" \
//!       --daemonize yes --dir /tmp
//!   done; sleep 1
//!   redis-cli --tls --cacert /tls/ca.crt --cluster create 127.0.0.1:37101 \
//!     127.0.0.1:37102 127.0.0.1:37103 --cluster-replicas 0 --cluster-yes
//!   tail -f /dev/null'
//! TEST_REDIS_CLUSTER_URL=rediss-cluster://127.0.0.1:37101 TEST_REDIS_CA_CERT=/tmp/tw-tls/ca.crt \
//!   cargo nextest run -p think-watch-test-support --test redis_cluster --run-ignored only
//! ```
//!
//! Keys carry fresh UUIDs, so nothing is flushed and runs don't collide.

use std::time::Duration;

use fred::clients::Client;
use fred::interfaces::ClientLike;
use fred::types::Builder;
use think_watch_test_support::prelude::*;
use think_watch_test_support::test_redis_config;

fn tls_url() -> Option<String> {
    let url = std::env::var("TEST_REDIS_TLS_URL").ok();
    if url.is_none() {
        eprintln!("TEST_REDIS_TLS_URL not set — skipping the Redis TLS test");
    }
    url
}

async fn connect(url: &str) -> Client {
    let config = test_redis_config(url);
    assert!(config.uses_rustls(), "{url} is not a TLS URL");
    let client = Builder::from_config(config).build().unwrap();
    client.init().await.unwrap();
    client
}

#[ignore = "integration test — needs TEST_REDIS_TLS_URL"]
#[tokio::test]
async fn the_limit_scripts_run_over_tls() {
    let Some(url) = tls_url() else { return };
    let redis = connect(&url).await;
    think_watch_test_support::redis_scripts::exercise_the_limit_scripts(&redis).await;
}

#[ignore = "integration test — needs TEST_REDIS_TLS_URL"]
#[tokio::test]
async fn a_certificate_the_roots_do_not_cover_is_refused() {
    // The test server's certificate comes from a private CA. Without
    // REDIS_CA_CERT only the system's roots are trusted, so the handshake
    // must fail: the certificate is checked, not merely accepted.
    let Some(url) = tls_url() else { return };
    let config = think_watch_common::redis_config::client_config(&url, None).unwrap();
    let client = Builder::from_config(config).build().unwrap();
    let init = tokio::time::timeout(Duration::from_secs(10), client.init())
        .await
        .expect("the handshake ends within 10 s");
    let err = init.expect_err("a certificate from an untrusted CA was accepted");
    // fred reports rustls's verdict as an I/O error; the rustls error
    // inside names the certificate.
    assert!(err.details().contains("InvalidCertificate"), "{err:?}");
}

#[ignore = "integration test — needs TEST_REDIS_TLS_URL"]
#[tokio::test]
async fn the_gateway_runs_over_tls() {
    let Some(url) = tls_url() else { return };
    let app = TestApp::try_spawn_with(SpawnOptions {
        redis_url: Some(url),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(app.state.config.redis_config().unwrap().uses_rustls());

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("tls-prov"),
        "openai",
        &mock.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let gw = app.gateway_client();
    let ready: Json = gw
        .get("/health/ready")
        .await
        .unwrap()
        .assert_ok()
        .json()
        .unwrap();
    assert_eq!(ready["redis"], true, "{ready}");

    // A request limit lives in Redis: two pass, the third is refused.
    fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        user.user.id,
        "ai_gateway",
        "requests",
        60,
        2,
    )
    .await
    .unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("tls-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    gw.set_bearer(&key.plaintext);
    let chat = json!({"model": "gpt-test",
                      "messages": [{"role": "user", "content": Uuid::new_v4().to_string()}]});
    for _ in 0..2 {
        gw.post("/v1/chat/completions", chat.clone())
            .await
            .unwrap()
            .assert_ok();
    }
    let r = gw.post("/v1/chat/completions", chat).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());

    // Config change notices between instances: a subscriber built the
    // way `init::spawn_config_subscriber` builds its three hears a
    // publish sent through the server's own client.
    think_watch_test_support::assert_config_notice_arrives(
        app.state.config.redis_config().unwrap(),
        &app.state.redis,
    )
    .await;
}
