//! Streaming PII restoration end-to-end.
//!
//! Unit tests in `pii_redactor.rs` cover `PiiStreamRestorer`'s
//! chunk-boundary buffering exhaustively (single-byte chunks,
//! trailing lone `{`, unknown placeholder pass-through, mid-
//! placeholder upstream truncation). What they don't cover:
//! whether the production proxy actually wires the restorer in
//! correctly when the upstream's SSE chunks split a placeholder
//! across event boundaries.
//!
//! Recipe:
//!   - Configure a PII pattern that matches `alice@example.com`.
//!   - Custom wiremock responder reads the gateway's outbound
//!     request, finds the `{{EMAIL_…}}` placeholder the redactor
//!     planted, and streams back SSE chunks with that exact
//!     placeholder split *inside* a `delta.content` field — one
//!     half in chunk N, the other half in chunk N+1.
//!   - Read the full SSE response on the client side and assert
//!     the reassembled content contains `alice@example.com`
//!     verbatim — neither the placeholder nor a garbled split.
//!
//! What this catches: a refactor that disables the restorer in
//! the streaming branch, or one that drops the buffering loop and
//! emits chunks as-is, would let the placeholder leak through to
//! the client (or worse, leak just half of it). Both are silent
//! at the unit-test level.

use serde_json::Value;
use std::sync::Arc;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn streaming_pii_restorer_reassembles_split_placeholder_in_response() {
    let app = TestApp::spawn().await;

    // 1. Configure a single PII rule for emails. The redactor
    //    inserts `{{EMAIL_<salt>_<counter>}}` into outbound
    //    messages and keeps the mapping in the per-request
    //    RedactionContext for the response side to restore.
    fixtures::set_setting(
        &app.db,
        "security.pii_redactor_patterns",
        json!([{
            "name": "email",
            "regex": r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}",
            "placeholder_prefix": "EMAIL"
        }]),
    )
    .await
    .unwrap();
    app.state.dynamic_config.reload().await.unwrap();
    let pii = think_watch_server::app::load_pii_redactor(&app.state.dynamic_config).await;
    app.state.pii_redactor.store(Arc::new(pii));

    // 2. Stand up a streaming wiremock that introspects the
    //    incoming request body, fishes the `{{EMAIL_…}}` token out
    //    of the user message, and splits IT (not the surrounding
    //    text) across two SSE chunks — exactly the boundary case
    //    `PiiStreamRestorer::safe_emit_boundary` is supposed to
    //    handle.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            // The redactor rewrites every "user" message in place.
            let placeholder = body["messages"]
                .as_array()
                .and_then(|arr| {
                    arr.iter().find_map(|m| {
                        m["content"].as_str().and_then(|s| {
                            // Find the canonical `{{EMAIL_<salt>_<n>}}` token.
                            let start = s.find("{{EMAIL_")?;
                            let rest = &s[start..];
                            let end = rest.find("}}")? + 2;
                            Some(rest[..end].to_string())
                        })
                    })
                })
                .expect("upstream did not see a `{{EMAIL_…}}` placeholder — redactor not engaged");

            // Split right after the prefix: half in the first chunk,
            // half in the second. The split point is INSIDE the
            // placeholder so the restorer must buffer across SSE
            // events to reassemble it.
            let mid = "{{EMAIL_".len();
            let head = &placeholder[..mid];
            let tail = &placeholder[mid..];

            let chunk_with = |delta: Value| {
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": "chatcmpl-pii-stream",
                        "object": "chat.completion.chunk",
                        "created": 1_700_000_000_i64,
                        "model": "pii-stream-test",
                        "choices": [{
                            "index": 0,
                            "delta": delta,
                            "finish_reason": null,
                        }],
                    })
                )
            };
            let final_chunk = format!(
                "data: {}\n\n",
                json!({
                    "id": "chatcmpl-pii-stream",
                    "object": "chat.completion.chunk",
                    "created": 1_700_000_000_i64,
                    "model": "pii-stream-test",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 4, "total_tokens": 9}
                })
            );
            // Pre-amble + first half of placeholder, then second half + post-amble.
            let mut sse = String::new();
            sse.push_str(&chunk_with(json!({"role": "assistant"})));
            sse.push_str(&chunk_with(
                json!({"content": format!("Your email is {head}")}),
            ));
            sse.push_str(&chunk_with(
                json!({"content": format!("{tail}, recorded.")}),
            ));
            sse.push_str(&final_chunk);
            sse.push_str("data: [DONE]\n\n");
            ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream")
        })
        .mount(&server)
        .await;

    // 3. Wire the gateway up against the mock.
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("pii-stream-prov"),
        "openai",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "pii-stream-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "pii-stream-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    // 4. Drive a streaming completion. The user's message contains
    //    the email; the redactor swaps it for a placeholder before
    //    forwarding; the upstream (above) echoes the placeholder
    //    split across SSE chunks; the gateway's
    //    `stream_to_sse_with_restorer` reassembles + restores
    //    before flushing to us.
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": "pii-stream-test",
                "messages": [{"role": "user", "content": "remember alice@example.com"}],
                "stream": true
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body = resp.text();

    // 5. The reassembled SSE body must contain the original email
    //    verbatim. Either failure mode shows up here:
    //      a. Restorer disabled → placeholder leaks to client.
    //      b. Restorer present but skipping the chunk-boundary
    //         buffer → garbage like `{{EMAIL_alice@example.com`.
    assert!(
        body.contains("alice@example.com"),
        "client never saw the restored email — placeholder leaked or split: {body}"
    );
    assert!(
        !body.contains("{{EMAIL_"),
        "raw placeholder leaked to the client: {body}"
    );
    assert!(
        body.contains("[DONE]"),
        "stream did not terminate cleanly: {body}"
    );
}
