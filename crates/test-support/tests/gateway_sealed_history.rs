//! A conversation whose history an upstream encrypted (OpenAI Responses
//! `encrypted_content`, sent with `store: false` as Codex does) goes back
//! to the provider that answered it: only that account can read it.
//!
//! Two providers serve the same model over Responses. The first turn of a
//! conversation (`x-session-id`) is answered by `a`; later turns carry what
//! `a` encrypted, while the routing would send them to `b` — by weight, or
//! by an affinity pointing there.

use fred::interfaces::KeysInterface;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

const MODEL: &str = "sealed-model";

/// A Responses upstream that answers with `tag` — buffered, or streamed
/// when asked to stream.
async fn responses_upstream(tag: &str) -> MockProvider {
    let mock = MockProvider {
        server: wiremock::MockServer::start().await,
    };
    let answer = json!({
        "id": format!("resp_{tag}"), "object": "response", "status": "completed",
        "model": MODEL,
        "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                    "content": [{"type": "output_text", "text": tag}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4},
    });
    mock.mount(
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                think_watch_test_support::mock_provider::responses_answer(MODEL, false),
                "text/event-stream",
            ))
            .with_priority(3),
    )
    .await;
    mock.mount(
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .with_priority(5),
    )
    .await;
    mock
}

/// From now on `upstream` fails every request with a 500.
async fn breaks(upstream: &MockProvider) {
    upstream
        .mount(
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "down"}})),
                )
                .with_priority(1),
        )
        .await;
}

/// OpenAI's refusal of content another account encrypted: as an answer,
/// and as the error event of a stream the Codex backend opens with 200.
async fn refuses_sealed_history(upstream: &MockProvider) {
    let failed = json!({"type": "response.failed", "response": {"id": "resp_r", "status": "failed",
        "error": {"code": "invalid_encrypted_content",
                  "message": "Encrypted function output content could not be decrypted or decoded."}}});
    upstream
        .mount(
            Mock::given(method("POST"))
                .and(body_partial_json(json!({"stream": true})))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    format!("event: response.failed\ndata: {failed}\n\n"),
                    "text/event-stream",
                ))
                .with_priority(1),
        )
        .await;
    upstream
        .mount(
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
                    "message": "The encrypted content for item rs_68 could not be verified.",
                    "type": "invalid_request_error", "param": null,
                    "code": "invalid_encrypted_content"}})))
                .with_priority(2),
        )
        .await;
}

async fn hits(upstream: &MockProvider) -> usize {
    upstream.received_requests().await.len()
}

struct Setup {
    app: TestApp,
    a: MockProvider,
    b: MockProvider,
    a_id: Uuid,
    a_name: String,
    b_id: Uuid,
    user_id: Uuid,
    key: String,
}

/// `a` serves [`MODEL`] over Responses; `b` joins once the first turn has
/// gone to `a` (see [`Setup::add_b`]).
async fn setup() -> Setup {
    let app = TestApp::spawn().await;
    let a = responses_upstream("from-a").await;
    let b = responses_upstream("from-b").await;
    let a_name = unique_name("acct-a");
    let pa = fixtures::create_provider(&app.db, &a_name, "openai", &a.uri(), None)
        .await
        .unwrap();
    let pb = fixtures::create_provider(&app.db, &unique_name("acct-b"), "openai", &b.uri(), None)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, pa.id, MODEL, 1)
        .await
        .unwrap();
    responses_routes(&app).await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(&app.db, user.user.id, "s", &["ai_gateway"], None, None)
        .await
        .unwrap();
    Setup {
        app,
        a,
        b,
        a_id: pa.id,
        a_name,
        b_id: pb.id,
        user_id: user.user.id,
        key: key.plaintext,
    }
}

async fn responses_routes(app: &TestApp) {
    sqlx::query(
        "UPDATE model_routes SET upstream_protocol = 'openai_responses' WHERE model_id = $1",
    )
    .bind(MODEL)
    .execute(&app.db)
    .await
    .unwrap();
    app.rebuild_gateway_router().await;
}

impl Setup {
    /// `b` joins with a weight a thousand times `a`'s.
    async fn add_b(&self) {
        fixtures::create_model_route(&self.app.db, self.b_id, MODEL, 1000)
            .await
            .unwrap();
        responses_routes(&self.app).await;
    }

    fn client(&self, session: Option<&str>) -> TestClient {
        let gw = self.app.gateway_client();
        gw.set_bearer(&self.key);
        if let Some(s) = session {
            gw.set_header("x-session-id", s);
        }
        gw
    }

    /// The provider-mode affinity of this user for the model points at `b`.
    async fn affinity_to_b(&self) {
        let key = format!("affinity:provider:{}:{MODEL}", self.user_id);
        let _: () = self
            .app
            .state
            .redis
            .set(&key, self.b_id.to_string(), None, None, false)
            .await
            .unwrap();
    }

    fn binding_key(&self, session: &str) -> String {
        format!("sealed_holder:{}:{session}", self.user_id)
    }

    /// The provider remembered for `session`, once it is (a stream's is
    /// written by its tail, after the response).
    async fn holder(&self, session: &str) -> Option<String> {
        for _ in 0..50 {
            let v: Option<String> = self
                .app
                .state
                .redis
                .get(self.binding_key(session))
                .await
                .unwrap();
            if v.is_some() {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        None
    }
}

fn first_turn(stream: bool) -> Json {
    json!({"model": MODEL, "store": false, "stream": stream, "input": "hi"})
}

/// A turn carrying a remote compaction `a` encrypted: no other provider
/// can read it, and it cannot be left out.
fn content_turn() -> Json {
    json!({"model": MODEL, "store": false, "input": [
        {"type": "compaction", "encrypted_content": "gAAAAABcompacted"},
        {"type": "message", "role": "user", "content": "go on"}
    ]})
}

/// A turn carrying only reasoning `a` encrypted.
fn reasoning_turn() -> Json {
    json!({"model": MODEL, "store": false, "input": [
        {"role": "user", "content": "hi"},
        {"type": "reasoning", "summary": [], "encrypted_content": "gAAAAABthought"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "x"}]},
        {"role": "user", "content": "go on"}
    ]})
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn encrypted_content_stays_with_its_provider_whatever_the_routing_prefers() {
    let s = setup().await;
    let gw = s.client(Some("conv-1"));

    // The first turn, streamed, goes to `a` — the only route so far.
    let resp = gw.post("/v1/responses", first_turn(true)).await.unwrap();
    resp.assert_ok();
    assert!(resp.text().contains("response.completed"));
    let holder = s
        .holder("conv-1")
        .await
        .expect("the answering provider is remembered");
    assert!(holder.starts_with(&s.a_id.to_string()), "{holder}");
    assert!(holder.ends_with(&s.a_name), "{holder}");

    // `b` is preferred by weight and by the affinity; the conversation's
    // history is `a`'s.
    s.add_b().await;
    s.affinity_to_b().await;
    for _ in 0..3 {
        gw.post("/v1/responses", content_turn())
            .await
            .unwrap()
            .assert_ok();
    }
    assert_eq!(hits(&s.a).await, 4);
    assert_eq!(hits(&s.b).await, 0, "sent where it cannot be read");

    // A conversation without a session id, or one nobody remembers, is
    // routed as before: the affinity sends it to `b`.
    s.affinity_to_b().await;
    s.client(None)
        .post("/v1/responses", content_turn())
        .await
        .unwrap()
        .assert_ok();
    let _: i64 = s
        .app
        .state
        .redis
        .del(s.binding_key("conv-1"))
        .await
        .unwrap();
    s.affinity_to_b().await;
    gw.post("/v1/responses", content_turn())
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(hits(&s.a).await, 4);
    assert_eq!(hits(&s.b).await, 2);
    // …and is remembered again by whoever answered it.
    let holder = s.holder("conv-1").await.unwrap();
    assert!(holder.starts_with(&s.b_id.to_string()), "{holder}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn encrypted_content_does_not_fail_over_to_a_provider_that_cannot_read_it() {
    let s = setup().await;
    let gw = s.client(Some("conv-2"));
    gw.post("/v1/responses", first_turn(false))
        .await
        .unwrap()
        .assert_ok();
    s.add_b().await;
    breaks(&s.a).await;

    // `a` answers with its own error, and nothing goes to `b` — buffered
    // or streamed.
    let resp = gw.post("/v1/responses", content_turn()).await.unwrap();
    resp.assert_status(500);
    let mut streamed = content_turn();
    streamed["stream"] = json!(true);
    let resp = gw.post("/v1/responses", streamed).await.unwrap();
    assert!(resp.text().contains("down"), "{}", resp.text());
    assert_eq!(hits(&s.a).await, 3);
    assert_eq!(hits(&s.b).await, 0);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn encrypted_content_whose_provider_is_gone_is_refused_up_front() {
    let s = setup().await;
    let gw = s.client(Some("conv-3"));
    gw.post("/v1/responses", first_turn(false))
        .await
        .unwrap()
        .assert_ok();
    s.add_b().await;
    sqlx::query("UPDATE model_routes SET enabled = false WHERE provider_id = $1")
        .bind(s.a_id)
        .execute(&s.app.db)
        .await
        .unwrap();
    s.app.rebuild_gateway_router().await;

    let resp = gw.post("/v1/responses", content_turn()).await.unwrap();
    resp.assert_status(409);
    let body: Json = resp.json().unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("encrypted by provider `{}`", s.a_name)),
        "{message}"
    );
    assert!(
        message.contains("no other provider can read it"),
        "{message}"
    );
    assert!(message.contains("Start a new conversation"), "{message}");
    assert_eq!(hits(&s.b).await, 0);

    // Reasoning alone does without it.
    gw.post("/v1/responses", reasoning_turn())
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(hits(&s.b).await, 1);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn encrypted_reasoning_prefers_its_provider_and_fails_over_without_it() {
    let s = setup().await;
    // Named the way Codex names its conversation, without `x-session-id`.
    let gw = s.client(None);
    gw.set_header("session_id", "conv-4");
    gw.post("/v1/responses", first_turn(false))
        .await
        .unwrap()
        .assert_ok();
    s.add_b().await;
    s.affinity_to_b().await;
    for _ in 0..3 {
        gw.post("/v1/responses", reasoning_turn())
            .await
            .unwrap()
            .assert_ok();
    }
    assert_eq!(hits(&s.a).await, 4);
    assert_eq!(hits(&s.b).await, 0);

    // `a` down: `b` takes it.
    breaks(&s.a).await;
    let resp = gw.post("/v1/responses", reasoning_turn()).await.unwrap();
    resp.assert_ok();
    let body: Json = resp.json().unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "from-b", "{body}");
    assert_eq!(hits(&s.a).await, 5);
    assert_eq!(hits(&s.b).await, 1);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_provider_that_cannot_read_the_history_is_said_to_plainly() {
    let s = setup().await;
    // Nobody remembers this conversation, and `a` cannot read it.
    refuses_sealed_history(&s.a).await;
    let gw = s.client(Some("conv-5"));

    let resp = gw.post("/v1/responses", content_turn()).await.unwrap();
    resp.assert_status(400);
    let body: Json = resp.json().unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "Provider `{}` cannot read this conversation's encrypted history",
            s.a_name
        )),
        "{message}"
    );
    assert!(
        message.contains("another provider encrypted it"),
        "{message}"
    );
    assert!(message.contains("start a new conversation"), "{message}");
    // The upstream's own words are kept.
    assert!(message.contains("could not be verified"), "{message}");

    // The same refusal partway through a stream.
    let mut streamed = content_turn();
    streamed["stream"] = json!(true);
    let resp = gw.post("/v1/responses", streamed).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(
        text.contains("cannot read this conversation's encrypted history"),
        "{text}"
    );
    assert!(text.contains("could not be decrypted"), "{text}");
    assert!(
        !text.contains("\"code\":\"invalid_encrypted_content\""),
        "the upstream's own frame went out: {text}"
    );

    // Neither answer is remembered as `a` holding the conversation.
    let held: Option<String> = s
        .app
        .state
        .redis
        .get(s.binding_key("conv-5"))
        .await
        .unwrap();
    assert_eq!(held, None);
}
