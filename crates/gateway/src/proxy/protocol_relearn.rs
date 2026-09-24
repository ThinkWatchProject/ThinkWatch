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
//! So the gateway retries the same route in one of its alternate
//! dialects (see `generate::send`) and, when one answers, writes the
//! working dialect back to the route. One request pays the retry; every
//! later request goes straight out on the right protocol.

use uuid::Uuid;

use crate::protocol::UpstreamProtocol;
use tw_types::GatewayError;

/// Does this failure look like "wrong dialect" rather than "bad
/// request" or "upstream down"?
///
/// Deliberately narrow. A false positive costs a wasted upstream call
/// and, worse, could persist a dialect that merely happened to answer
/// once. Only 4xx bodies that name an API surface qualify — the shape
/// upstreams actually use to report this.
pub(super) fn is_protocol_mismatch(err: &GatewayError) -> bool {
    let GatewayError::ProviderHttpError { status, message } = err else {
        return false;
    };
    // A 5xx is an upstream incident, not a dialect problem, and retrying
    // it through another dialect would misattribute an outage.
    if !(400..500).contains(status) {
        return false;
    }
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
}
