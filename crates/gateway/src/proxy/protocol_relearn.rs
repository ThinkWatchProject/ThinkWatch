//! Recovering when an upstream rejects the wire dialect we chose.
//!
//! `model_routes.upstream_protocol` is decided once, at import time,
//! from a probe (see `think_watch_server::protocol_probe`). Upstreams
//! change: a model gets moved onto a different API, an aggregator
//! retires a dialect, or the probe's family grouping lumped together
//! two models that don't actually share a surface. When that happens
//! the upstream tells us plainly — "the model X does not support the
//! '/v1/chat/completions' API" — and there's no reason to make an
//! operator read the log and go fix a setting.
//!
//! So the gateway retries the same route through one of its
//! pre-built alternate adapters and, when one answers, writes the
//! working dialect back to the route. One request pays the retry; every
//! later request goes straight out on the right protocol.

use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::providers::protocol::UpstreamProtocol;
use crate::providers::traits::{ChatCompletionChunk, ChatCompletionRequest, GatewayError};
use crate::router::RouteEntry;

/// Does this failure look like "wrong dialect" rather than "bad
/// request" or "upstream down"?
///
/// Deliberately narrow. A false positive costs a wasted upstream call
/// and, worse, could persist a dialect that merely happened to answer
/// once. Only 4xx bodies that name an API surface qualify — the shape
/// upstreams actually use to report this.
pub(super) fn is_protocol_mismatch(err: &GatewayError) -> bool {
    let message = match err {
        GatewayError::ProviderHttpError { status, message } => {
            if !(400..500).contains(status) {
                return false;
            }
            message
        }
        // `ProviderBase::check_status` reports most non-2xx upstream
        // replies as `ProviderError("{label} returned {status}: {body}")`
        // rather than the structured variant, so the status has to be
        // read back out of the text. Matching only the structured shape
        // would mean this never fires in production.
        GatewayError::ProviderError(message) => {
            if !mentions_4xx(message) {
                return false;
            }
            message
        }
        _ => return false,
    };
    let m = message.to_ascii_lowercase();
    let names_an_api = m.contains("/v1/chat/completions")
        || m.contains("/v1/messages")
        || m.contains("/v1/responses")
        || m.contains("chat/completions")
        || m.contains("generatecontent");
    let sounds_unsupported = m.contains("not support")
        || m.contains("unsupported")
        || m.contains("not available")
        || m.contains("invalid endpoint")
        || m.contains("unknown endpoint");
    names_an_api && sounds_unsupported
}

/// Open a stream against `entry`, recovering from a rejected dialect
/// the same way the buffered path does.
///
/// A stream can't be retried once bytes have reached the client — but a
/// dialect rejection arrives as the *first* item, before any chunk. So
/// peek that item, and if it's a mismatch, reopen against the route's
/// alternates and push the peeked item back onto the front.
///
/// All of that happens *inside* the returned stream, on first poll, not
/// before it is returned. Awaiting the first item up front would hold
/// the response headers until the first token arrived, which changes
/// time-to-headers for every streaming request and breaks the
/// client-disconnect accounting that depends on the response having
/// already started.
///
/// Without this, a client that only ever streams would keep paying the
/// failed first attempt forever, since nothing would ever record the
/// working dialect.
pub(super) fn open_stream_with_relearn(
    entry: &RouteEntry,
    request: ChatCompletionRequest,
    db: sqlx::PgPool,
) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
    // Own everything the stream needs: it outlives this call, and all
    // of it is Arc-cheap to clone.
    let primary = Arc::clone(&entry.provider);
    let alternates = entry.alternates.clone();
    let route_id = entry.route_id;
    let configured = entry.protocol;
    let provider_name = entry.provider_name.clone();

    Box::pin(async_stream::stream! {
        let mut inner = primary.stream_chat_completion(request.clone());
        let mut first = inner.next().await;

        if let Some(Err(ref e)) = first
            && is_protocol_mismatch(e)
        {
            for (protocol, adapter) in &alternates {
                tracing::info!(
                    provider = %provider_name,
                    model = %request.model,
                    from = %configured,
                    to = %protocol,
                    "Upstream rejected the configured protocol on a stream — retrying with an alternate"
                );
                let mut retry = adapter.stream_chat_completion(request.clone());
                let retry_first = retry.next().await;
                let recovered = !matches!(retry_first, Some(Err(_)));
                let keep_going =
                    matches!(retry_first, Some(Err(ref e)) if is_protocol_mismatch(e));
                first = retry_first;
                inner = retry;
                if recovered {
                    persist(&db, route_id, *protocol).await;
                    break;
                }
                if !keep_going {
                    break;
                }
            }
        }

        // `None` here means the upstream produced nothing at all — the
        // empty stream is the faithful representation and the pump
        // handles it.
        if let Some(item) = first {
            yield item;
            while let Some(next) = inner.next().await {
                yield next;
            }
        }
    })
}

/// Pull the HTTP status back out of a `check_status` message
/// (`"OpenAI returned 400 Bad Request: …"`) and report whether it's a
/// 4xx. A 5xx is an upstream incident, not a dialect problem, and
/// retrying it through another dialect would misattribute an outage.
fn mentions_4xx(message: &str) -> bool {
    message
        .split(" returned ")
        .skip(1)
        .filter_map(|rest| rest.get(..3).and_then(|s| s.parse::<u16>().ok()))
        .any(|status| (400..500).contains(&status))
}

/// Persist a relearned dialect so it survives the next router rebuild.
///
/// Best-effort on purpose: the in-memory retry already produced a good
/// response for the caller, and failing their request because a
/// bookkeeping UPDATE didn't land would be a strictly worse outcome.
/// A lost write costs one more retry on the next request.
pub(super) async fn persist(db: &sqlx::PgPool, route_id: Uuid, protocol: UpstreamProtocol) {
    // Update the import-time probe cache from the same statement: a
    // live call is better evidence than a probe, and leaving a stale
    // "unavailable" behind would keep the model out of the import
    // picker even though it just answered.
    let result = sqlx::query(
        r#"WITH updated AS (
               UPDATE model_routes SET upstream_protocol = $2
                WHERE id = $1
            RETURNING provider_id, upstream_model
           )
           INSERT INTO provider_model_probes
               (provider_id, upstream_model, status, protocol, error, checked_at)
           SELECT provider_id, upstream_model, 'ok', $2, NULL, now() FROM updated
           ON CONFLICT (provider_id, upstream_model) DO UPDATE
               SET status = 'ok',
                   protocol = EXCLUDED.protocol,
                   error = NULL,
                   checked_at = now()"#,
    )
    .bind(route_id)
    .bind(protocol.as_str())
    .execute(db)
    .await;
    match result {
        Ok(_) => tracing::info!(
            %route_id,
            protocol = %protocol,
            "Relearned upstream protocol — route updated"
        ),
        Err(e) => tracing::warn!(
            %route_id,
            protocol = %protocol,
            "Relearned upstream protocol but could not persist it: {e}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_upstream_saying_wrong_api() {
        assert!(is_protocol_mismatch(&GatewayError::ProviderHttpError {
            status: 400,
            message: "The model 'anthropic.claude-opus-4' does not support the \
                      '/v1/chat/completions' API"
                .into(),
        }));
        assert!(is_protocol_mismatch(&GatewayError::ProviderHttpError {
            status: 404,
            message: "unsupported endpoint /v1/responses for this model".into(),
        }));
    }

    #[test]
    fn ignores_failures_that_are_not_about_the_dialect() {
        // A bad payload is the caller's problem — retrying it through
        // another dialect would just fail differently and, if it ever
        // succeeded, would persist a lie.
        assert!(!is_protocol_mismatch(&GatewayError::ProviderHttpError {
            status: 400,
            message: "messages: at least one message is required".into(),
        }));
        // Upstream incident, not a dialect problem.
        assert!(!is_protocol_mismatch(&GatewayError::ProviderHttpError {
            status: 503,
            message: "/v1/chat/completions is not available right now".into(),
        }));
        assert!(!is_protocol_mismatch(&GatewayError::UpstreamAuthError));
    }

    #[test]
    fn reads_the_status_out_of_the_check_status_message_shape() {
        // What upstream 4xx failures actually look like in production —
        // `check_status` formats them into `ProviderError`.
        assert!(is_protocol_mismatch(&GatewayError::ProviderError(
            "OpenAI returned 400 Bad Request: {\"message\":\"The model 'anthropic.claude-x' \
             does not support the '/v1/chat/completions' API\"}"
                .into()
        )));
        // 5xx in the same wrapper is an outage — not something another
        // dialect would fix.
        assert!(!is_protocol_mismatch(&GatewayError::ProviderError(
            "OpenAI returned 503 Service Unavailable: /v1/chat/completions not available".into()
        )));
        // No status at all ⇒ nothing to classify on.
        assert!(!is_protocol_mismatch(&GatewayError::ProviderError(
            "does not support the '/v1/chat/completions' API".into()
        )));
    }
}
