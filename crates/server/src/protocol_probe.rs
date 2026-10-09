//! Finding out, before a route exists, whether an upstream will
//! actually serve a model — and over which wire dialect.
//!
//! Aggregators list models in `/v1/models` that they will not serve to
//! your credential, and serve the ones they do over different APIs
//! depending on the model. Neither fact is discoverable from the
//! catalog, and neither is something an admin should have to research.
//! So each model gets probed with the smallest possible completion
//! before it is imported: refused models never become routes, and
//! served ones land with their dialect already recorded.
//!
//! Probing is per model, never per family. Grouping by vendor prefix
//! was the obvious optimisation and it is wrong: on a real upstream,
//! `minimax.minimax-m2.1` answers while `minimax.minimax-m2` refuses,
//! and `openai.gpt-oss-120b` answers while `openai.gpt-5.5` refuses —
//! same host, same path, same credential.
//!
//! Verdicts are cached in `provider_model_probes` with no expiry. They
//! are revisited only on an explicit operator action, when a live call
//! contradicts them, or when the provider's endpoint changes.

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;
use think_watch_gateway::call_ctx::CallCtx;
use think_watch_gateway::error::GatewayError;
use think_watch_gateway::protocol::UpstreamProtocol;
use uuid::Uuid;

use crate::gateway_adapters::{ProviderMaterials, build_upstream};

/// How many models we probe at once. The upstream is someone else's
/// service — this is deliberately modest, and it still resolves a
/// typical import in about one round trip.
const PROBE_CONCURRENCY: usize = 10;

/// Per-model ceiling. A model that hasn't answered by now is left
/// unrecorded rather than holding up the import: the route gets created
/// and the runtime relearn path sorts it out on first use, which is
/// exactly the pre-probe behaviour.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What we know about one `(provider, upstream model)` pair.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    /// The upstream served it over this dialect.
    Ok(UpstreamProtocol),
    /// Every candidate dialect was refused. Carries the upstream's own
    /// wording — the admin needs that to know whether to enable the
    /// model upstream or give up on it.
    Unavailable(String),
    /// The probe itself didn't conclude (timeout, network). Not a
    /// statement about the upstream, so it is never cached.
    Unknown,
}

impl Verdict {
    pub(crate) fn protocol(&self) -> Option<UpstreamProtocol> {
        match self {
            Self::Ok(p) => Some(*p),
            _ => None,
        }
    }
}

/// Read cached verdicts for a provider. Models with no row come back
/// absent, not `Unknown` — "never asked" and "asked, got nothing" are
/// different things to the caller.
pub(crate) async fn cached_verdicts(
    db: &sqlx::PgPool,
    provider_id: Uuid,
) -> Result<HashMap<String, Verdict>, AppError> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT upstream_model, status, protocol, error
           FROM provider_model_probes WHERE provider_id = $1",
    )
    .bind(provider_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(model, status, protocol, error)| {
            let verdict = match status.as_str() {
                "ok" => Verdict::Ok(protocol.as_deref().and_then(UpstreamProtocol::parse)?),
                "unavailable" => Verdict::Unavailable(error.unwrap_or_default()),
                _ => return None,
            };
            Some((model, verdict))
        })
        .collect())
}

/// Record a verdict. `Unknown` is dropped on the floor by design — see
/// the enum.
pub(crate) async fn record(
    db: &sqlx::PgPool,
    provider_id: Uuid,
    upstream_model: &str,
    verdict: &Verdict,
) {
    let (status, protocol, error) = match verdict {
        Verdict::Ok(p) => ("ok", Some(p.as_str()), None),
        Verdict::Unavailable(msg) => ("unavailable", None, Some(msg.as_str())),
        Verdict::Unknown => return,
    };
    // Best-effort: the caller already has the answer it needs, and
    // failing an import over a cache write would be a worse outcome
    // than probing this model again next time.
    if let Err(e) = sqlx::query(
        r#"INSERT INTO provider_model_probes
               (provider_id, upstream_model, status, protocol, error, checked_at)
           VALUES ($1, $2, $3, $4, $5, now())
           ON CONFLICT (provider_id, upstream_model) DO UPDATE
               SET status = EXCLUDED.status,
                   protocol = EXCLUDED.protocol,
                   error = EXCLUDED.error,
                   checked_at = now()"#,
    )
    .bind(provider_id)
    .bind(upstream_model)
    .bind(status)
    .bind(protocol)
    .bind(error)
    .execute(db)
    .await
    {
        tracing::warn!(%provider_id, model = %upstream_model, "Could not cache probe verdict: {e}");
    }
}

/// Forget everything we learned about a provider's models.
///
/// Called when the endpoint or credentials change: every verdict was a
/// statement about the upstream that used to be there.
pub(crate) async fn clear_for_provider(db: &sqlx::PgPool, provider_id: Uuid) -> u64 {
    sqlx::query("DELETE FROM provider_model_probes WHERE provider_id = $1")
        .bind(provider_id)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or_default()
}

/// The smallest completion that still exercises the real code path: one
/// token out, one word in — encoded for `protocol` by the same layer that
/// converts live traffic. A hand-written probe body drifts from what
/// forwarding actually sends, and then "the probe passed, forwarding
/// fails" has nothing to go on.
///
/// Without the prompt-cache breakpoints the conversion adds for Claude,
/// though: a probe is far too short to be cached, and an upstream that
/// does not take them would refuse the probe and have the model recorded
/// as unavailable for good. Live traffic recovers from that refusal on
/// its own (see the gateway's `proxy::cache_marks`).
fn probe_request(
    upstream_model: &str,
    protocol: UpstreamProtocol,
    official: bool,
) -> tw_dialect::convert::Prepared {
    use tw_dialect::ir::{Message, Part, Request, Role, Target};
    let mut prepared = tw_dialect::convert::encode(
        &Request {
            model: upstream_model.to_string(),
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("hi".to_string())],
            }],
            max_tokens: Some(1),
            ..Default::default()
        },
        &Target {
            dialect: protocol.dialect(),
            official,
            default_max_tokens: 1,
        },
    );
    if let Some(body) = tw_dialect::cache::strip_marks(protocol.dialect(), &prepared.body) {
        prepared.body = body;
    }
    prepared
}

/// Does this failure tell us anything about the model, or only about
/// the moment?
///
/// Auth, rate-limit and transport failures say nothing — recording
/// "unavailable" off the back of an expired key would blacklist an
/// entire catalog for good, since verdicts don't expire. (A refused
/// credential can still come to say something: see [`settle`].)
fn is_inconclusive(err: &GatewayError) -> bool {
    match err {
        GatewayError::UpstreamAuthError { .. }
        | GatewayError::UpstreamRateLimited { .. }
        | GatewayError::NetworkError(_)
        | GatewayError::ProviderTimeout(_) => true,
        // An upstream incident is about the moment too.
        GatewayError::ProviderHttpError { status, .. } => *status >= 500 || *status == 408,
        _ => false,
    }
}

/// What probing one model found.
#[derive(Debug, PartialEq)]
enum Probe {
    Verdict(Verdict),
    /// The upstream refused the credential for this model (401/403): the
    /// attempts' wording, the refusal last. On its own that says nothing
    /// about the model — see [`settle`].
    CredentialRefused(Vec<String>),
}

/// Probe one model across its candidate dialects, first answer wins.
async fn probe_one(materials: &ProviderMaterials, upstream_model: &str) -> Probe {
    let candidates = UpstreamProtocol::candidates_for(&materials.provider_type, upstream_model);
    // Every refusal, not just the last one. The verdict is "no dialect
    // works", and reporting a single attempt's wording invites the
    // reader to conclude the other dialects were never tried — which is
    // exactly the wrong lesson, since the fix for a genuinely
    // dialect-specific failure is different from the fix for "this
    // upstream won't serve you this model at all".
    let mut refusals: Vec<String> = Vec::new();
    let upstream = build_upstream(materials);

    for candidate in candidates {
        let probe = probe_request(upstream_model, candidate, upstream.is_official());
        let attempt = tokio::time::timeout(
            PROBE_TIMEOUT,
            // The probe has no caller: it runs from the admin import path,
            // not from a user request. An empty CallCtx is the honest
            // representation — header templates resolve to blanks and no
            // trace id is forwarded, because there is no trace to join.
            upstream.send(
                probe.body,
                &probe.path,
                probe.query.as_deref(),
                candidate.dialect(),
                &[],
                &CallCtx::default(),
            ),
        )
        .await;

        match attempt {
            Ok(Ok(resp)) => {
                // Drain it so the connection goes back to the pool.
                let _ = resp.bytes().await;
                return Probe::Verdict(Verdict::Ok(candidate));
            }
            Ok(Err(GatewayError::UpstreamAuthError { status, message })) => {
                refusals.push(format!("{candidate}: HTTP {status}: {message}"));
                return Probe::CredentialRefused(refusals);
            }
            Ok(Err(e)) if is_inconclusive(&e) => {
                tracing::warn!(
                    provider = %materials.name,
                    model = %upstream_model,
                    "Probe inconclusive — not recording a verdict: {e}"
                );
                return Probe::Verdict(Verdict::Unknown);
            }
            Ok(Err(e)) => refusals.push(format!("{candidate}: {e}")),
            Err(_elapsed) => {
                tracing::warn!(
                    provider = %materials.name,
                    model = %upstream_model,
                    protocol = %candidate,
                    "Probe timed out"
                );
                return Probe::Verdict(Verdict::Unknown);
            }
        }
    }

    if refusals.is_empty() {
        // No candidates at all shouldn't happen, but "we learned
        // nothing" is the honest reading if it does.
        return Probe::Verdict(Verdict::Unknown);
    }
    Probe::Verdict(Verdict::Unavailable(unavailable_reason(&refusals)))
}

/// The verdict a probe comes to, once it is known whether the provider's
/// credential is good at all.
///
/// A refused credential says nothing about a model while the credential
/// might be what is wrong: an expired key refuses every model, and
/// verdicts don't expire. Known to be good, it was refused for this
/// model alone — access to it isn't granted, or a policy denies it — and
/// that is as much a verdict as any other refusal.
fn settle(probe: Probe, credential_good: bool) -> Verdict {
    match probe {
        Probe::Verdict(verdict) => verdict,
        Probe::CredentialRefused(refusals) if credential_good => {
            Verdict::Unavailable(unavailable_reason(&refusals))
        }
        Probe::CredentialRefused(_) => Verdict::Unknown,
    }
}

/// Is the provider's credential good, whatever it was refused for?
///
/// Only Bedrock can say: its control plane answers to the same
/// credential its models do, so a listing it serves proves the credential
/// good. An OpenAI-style `/v1/models` proves nothing — some serve it to
/// anyone. For any other provider a refusal stays inconclusive.
async fn is_credential_good(materials: &ProviderMaterials) -> bool {
    if materials.provider_type != "bedrock" {
        return false;
    }
    let Ok(endpoint) = crate::bedrock_catalog::endpoint(&materials.base_url) else {
        return false;
    };
    let upstream = build_upstream(materials);
    let check = crate::bedrock_catalog::accepts_credential(&upstream.client, &endpoint, &upstream);
    matches!(tokio::time::timeout(PROBE_TIMEOUT, check).await, Ok(true))
}

/// Compose the wording an operator reads when a model is refused.
///
/// Names every dialect that was tried. The reader's next question is
/// always "did it even try the right API?", and a message quoting one
/// attempt leaves that open.
fn unavailable_reason(refusals: &[String]) -> String {
    format!("refused on every supported API ({})", refusals.join(" | "))
}

/// Resolve a verdict for each model: cached ones for free, the rest
/// probed concurrently.
///
/// Synchronous on purpose. The caller is about to decide which routes
/// to create, and that decision needs the answer — a background probe
/// would mean creating routes first and discovering they're dead later,
/// which is the failure mode this whole mechanism exists to remove. The
/// wait is bounded by [`PROBE_TIMEOUT`] per model with
/// [`PROBE_CONCURRENCY`] in flight, and cached models cost nothing, so
/// a repeat import returns immediately.
pub(crate) async fn resolve(
    db: &sqlx::PgPool,
    provider: &Provider,
    encryption_key: &str,
    upstream_models: &[String],
    force: bool,
) -> HashMap<String, Verdict> {
    let mut out: HashMap<String, Verdict> = HashMap::new();
    let cached = if force {
        HashMap::new()
    } else {
        cached_verdicts(db, provider.id).await.unwrap_or_else(|e| {
            tracing::warn!(provider = %provider.name, "Could not read probe cache: {e}");
            HashMap::new()
        })
    };

    let mut to_probe: Vec<String> = Vec::new();
    for model in upstream_models {
        match cached.get(model) {
            Some(v) => {
                out.insert(model.clone(), v.clone());
            }
            None => to_probe.push(model.clone()),
        }
    }
    if to_probe.is_empty() {
        return out;
    }

    let materials = ProviderMaterials::from_provider(provider, encryption_key);
    let materials = &materials;
    let probed: Vec<(String, Probe)> = stream::iter(to_probe)
        .map(move |model| async move {
            let probe = probe_one(materials, &model).await;
            (model, probe)
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;

    // Asked once for the whole batch, and only when it matters
    let credential_good = probed
        .iter()
        .any(|(_, probe)| matches!(probe, Probe::CredentialRefused(_)))
        && is_credential_good(materials).await;

    for (model, probe) in probed {
        if let Probe::CredentialRefused(refusals) = &probe
            && !credential_good
        {
            tracing::warn!(
                provider = %provider.name,
                model = %model,
                "Probe inconclusive — the credential was refused: {}",
                refusals.join(" | ")
            );
        }
        let verdict = settle(probe, credential_good);
        record(db, provider.id, &model, &verdict).await;
        match &verdict {
            Verdict::Ok(p) => tracing::info!(
                provider = %provider.name, model = %model, protocol = %p,
                "Model probed — upstream serves it"
            ),
            Verdict::Unavailable(reason) => tracing::info!(
                provider = %provider.name, model = %model, reason = %reason,
                "Model probed — upstream refuses it on every dialect"
            ),
            Verdict::Unknown => {}
        }
        out.insert(model, verdict);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion marks Claude-bound requests for caching; a probe goes
    /// out without the marks, so an upstream that does not take them
    /// cannot make a model look unavailable.
    #[test]
    fn a_probe_carries_no_cache_breakpoints() {
        for (protocol, model) in [
            (UpstreamProtocol::AnthropicMessages, "claude-sonnet-4-5"),
            (
                UpstreamProtocol::BedrockNative,
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            ),
        ] {
            let body = String::from_utf8(probe_request(model, protocol, true).body).unwrap();
            assert!(
                !body.contains("cache_control") && !body.contains("cachePoint"),
                "{body}"
            );
            assert!(body.contains("hi"), "{body}");
        }
    }

    #[test]
    fn transient_failures_never_become_a_permanent_verdict() {
        // Verdicts don't expire, so recording "unavailable" off an
        // expired key would blacklist a whole catalog for good.
        assert!(is_inconclusive(&GatewayError::UpstreamAuthError {
            status: 401,
            message: "OpenAI: invalid key".into(),
        }));
        assert!(is_inconclusive(&GatewayError::UpstreamRateLimited {
            retry_after_secs: None
        }));
        assert!(is_inconclusive(&GatewayError::NetworkError(
            "connection reset".into()
        )));
        assert!(is_inconclusive(&GatewayError::ProviderHttpError {
            status: 503,
            message: "OpenAI: overloaded".into(),
        }));
    }

    #[test]
    fn a_refused_credential_is_a_verdict_only_once_the_credential_is_known_good() {
        let refused = || {
            Probe::CredentialRefused(vec![
                "bedrock_native: HTTP 403: Bedrock: {\"message\":\"You don't have access to \
                 the model with the specified model ID.\"}"
                    .into(),
            ])
        };
        // An expired key refuses every model: nothing is learned about any
        assert_eq!(settle(refused(), false), Verdict::Unknown);
        // A good credential refused for this model: it is out of reach
        let Verdict::Unavailable(reason) = settle(refused(), true) else {
            panic!("a refusal of a good credential is a verdict");
        };
        assert!(reason.contains("You don't have access to"), "{reason}");
        // Everything else is what the probe found
        assert_eq!(
            settle(Probe::Verdict(Verdict::Unknown), true),
            Verdict::Unknown
        );
    }

    #[test]
    fn a_refusal_of_the_model_itself_is_conclusive() {
        // This is the case worth recording: the upstream answered, and
        // its answer was "not this model".
        assert!(!is_inconclusive(&GatewayError::ProviderHttpError {
            status: 400,
            message: "OpenAI: The model 'openai.gpt-5.5' does not support \
                      the '/v1/chat/completions' API"
                .into(),
        }));
        assert!(!is_inconclusive(&GatewayError::ProviderHttpError {
            status: 404,
            message: "no such model".into(),
        }));
    }

    #[test]
    fn the_reason_names_every_dialect_that_was_tried() {
        // An operator seeing only the last attempt would reasonably
        // conclude the others were never tried, and go looking for a
        // protocol fix that doesn't exist.
        let reason = unavailable_reason(&[
            "openai_chat: does not support the '/v1/chat/completions' API".to_string(),
            "openai_responses: does not support the '/v1/responses' API".to_string(),
        ]);
        assert!(reason.contains("openai_chat"), "{reason}");
        assert!(reason.contains("openai_responses"), "{reason}");
        assert!(reason.contains("every supported API"), "{reason}");
    }

    #[test]
    fn verdict_exposes_the_protocol_only_when_served() {
        assert_eq!(
            Verdict::Ok(UpstreamProtocol::AnthropicMessages).protocol(),
            Some(UpstreamProtocol::AnthropicMessages)
        );
        assert_eq!(Verdict::Unavailable("refused".into()).protocol(), None);
        assert_eq!(Verdict::Unknown.protocol(), None);
    }
}
