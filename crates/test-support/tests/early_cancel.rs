//! A client that leaves before its response exists still leaves one
//! `gateway_logs` row: status 499, `client_cancelled`, no tokens, no
//! cost.
//!
//! Before, only a stream that had started recorded a disconnect. A
//! client that left while the key's roles loaded, the limits ran, a
//! route was picked or a whole answer was awaited left no trace: hyper
//! dropped the handler and nothing after the await point ran.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A user, a key, and `model` routed to an upstream that takes a minute
/// to answer.
async fn slow_route(app: &TestApp, model: &str) -> (MockServer, Uuid, String) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices": []}))
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&server)
        .await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("early-cancel"),
        "openai",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "ec", &["ai_gateway"], None, None)
        .await
        .unwrap();
    (server, user.user.id, key.plaintext)
}

/// The user's rows, once `want` of them have landed (or after ~10 s).
async fn rows(app: &TestApp, user_id: Uuid, want: usize) -> Vec<(i64, i64, i64, String)> {
    let ch = app.state.clickhouse.as_ref().expect("CH wired up");
    let mut found = Vec::new();
    for _ in 0..100 {
        found = ch
            .query(
                "SELECT ifNull(status_code, -1), ifNull(input_tokens, -1), \
                        ifNull(output_tokens, -1), ifNull(detail, '') \
                   FROM gateway_logs WHERE user_id = ?",
            )
            .bind(user_id.to_string())
            .fetch_all::<(i64, i64, i64, String)>()
            .await
            .expect("CH query");
        if found.len() >= want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    found
}

fn assert_cancelled(row: &(i64, i64, i64, String)) {
    let (status, input, output, detail) = row;
    assert_eq!(*status, 499, "{detail}");
    assert_eq!((*input, *output), (0, 0), "{detail}");
    let detail: Value = serde_json::from_str(detail).unwrap();
    assert_eq!(detail["stream_outcome"], "client_cancelled", "{detail}");
    assert_eq!(detail["cancelled_before"], "response", "{detail}");
}

/// Held deterministically: the client goes once the upstream has the
/// request, while the gateway waits for a whole answer.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_client_that_leaves_while_a_whole_answer_is_awaited_is_recorded() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (server, user_id, key) = slow_route(&app, "early-cancel-whole").await;

    let url = format!("{}/v1/chat/completions", app.gateway_url);
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .bearer_auth(key)
            .json(&json!({"model": "early-cancel-whole",
                          "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
    });
    while server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        tokio::task::yield_now().await;
    }
    call.abort();

    let found = rows(&app, user_id, 1).await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_cancelled(&found[0]);
    let detail: Value = serde_json::from_str(&found[0].3).unwrap();
    assert_eq!(detail["model_id"], "early-cancel-whole", "{detail}");
}

/// Leaving before routing ends: the middleware loads the key's roles
/// right after it arms the guard, so a request held there is known but
/// not yet routed. A lock on the role assignments holds it, and the
/// client goes while it waits. One cancelled row, written as it leaves,
/// and nothing more once the lock is let go. Never a success, never two.
///
/// A client that leaves before its key is even looked up is nobody yet
/// and writes nothing. Five clients on a 5 ms timer used to stand in for
/// this one, and on a slow runner all five could leave that early.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_client_that_leaves_before_routing_ends_leaves_at_most_one_cancelled_row() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (_server, user_id, key) = slow_route(&app, "early-cancel-quick").await;

    // Nothing else in this test reads role assignments, so whatever waits
    // on the lock is the request.
    let mut roles = app.db.begin().await.unwrap();
    sqlx::query("LOCK TABLE rbac_role_assignments IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *roles)
        .await
        .unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *roles)
        .await
        .unwrap();

    let url = format!("{}/v1/chat/completions", app.gateway_url);
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .bearer_auth(key)
            .json(&json!({"model": "early-cancel-quick", "stream": true,
                          "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                             WHERE $1 = ANY(pg_blocking_pids(pid)))",
        )
        .bind(holder)
        .fetch_one(&app.db)
        .await
        .unwrap();
        if waiting {
            break;
        }
        assert!(
            !call.is_finished() && tokio::time::Instant::now() < deadline,
            "the request never reached the key's roles"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    call.abort();

    // Written as the client left: the request is still held.
    let found = rows(&app, user_id, 1).await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_cancelled(&found[0]);

    // Once the lock is let go, a request that outlived its client would
    // carry on and record again.
    roles.rollback().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(rows(&app, user_id, 1).await, found);
}

/// A stream that started records its own cancel; the guard is disarmed
/// by then, so there is one row, not two.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_started_stream_that_is_left_is_recorded_once() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (_server, user_id, key) = slow_route(&app, "early-cancel-stream").await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.gateway_url))
        .bearer_auth(&key)
        .json(&json!({"model": "early-cancel-stream", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("the stream starts");
    assert_eq!(resp.status(), 200);
    drop(resp);

    let found = rows(&app, user_id, 1).await;
    assert_eq!(found.len(), 1, "{found:?}");
    let detail: Value = serde_json::from_str(&found[0].3).unwrap();
    assert_eq!(detail["stream_outcome"], "client_cancelled", "{detail}");
    assert!(detail.get("cancelled_before").is_none(), "{detail}");

    // No second row turns up later.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(rows(&app, user_id, 1).await.len(), 1);
}
