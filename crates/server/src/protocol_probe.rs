//! Working out which wire dialect an upstream wants for a given model,
//! without ever asking the admin.
//!
//! Aggregators routinely serve several model families over one host and
//! one credential while exposing a *different* API per family. The
//! admin shouldn't have to know that, let alone encode it as one
//! provider record per dialect. So the gateway determines it itself, in
//! three escalating steps:
//!
//! 1. **Catalog metadata** — some upstreams already say which endpoints
//!    a model answers on. Free, so it's tried first.
//! 2. **Model-id family heuristic** — orders the candidates
//!    (`anthropic.*` → Messages first). Never decides on its own; it
//!    only chooses what to try first.
//! 3. **Live probe** — the smallest possible completion against each
//!    candidate in turn. The first one that answers wins.
//!
//! Results are memoised per model *family* within a run, so importing
//! 55 models across 6 families costs 6 probes, not 55.
//!
//! Anything left unresolved stays NULL in `model_routes` and is picked
//! up later by the runtime relearn path — a failed probe degrades to
//! "figure it out on first use", never to a hard error.

use std::collections::HashMap;

use think_watch_common::models::Provider;
use think_watch_gateway::providers::protocol::UpstreamProtocol;
use think_watch_gateway::providers::traits::{ChatCompletionRequest, ChatMessage, GatewayError};

use crate::gateway_adapters::{ProviderMaterials, build_adapter};

/// Group key for memoising probe results. Aggregator ids are
/// vendor-prefixed (`anthropic.claude-…`, `openai.gpt-…`); first-party
/// ones aren't (`claude-…`, `gpt-…`), so fall back to the leading
/// alphabetic run. Two models that share this key are assumed to share
/// an API surface — if that assumption is ever wrong for a specific
/// model, the runtime relearn path corrects that route on first use.
fn family_key(upstream_model: &str) -> String {
    let lower = upstream_model.to_ascii_lowercase();
    if let Some((vendor, _)) = lower.split_once(['.', '/']) {
        return vendor.to_string();
    }
    lower
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect()
}

/// Read the protocol straight off a catalog entry, when the upstream
/// publishes one. Recognises the shapes seen in the wild: a list of
/// endpoint paths, or a list of API names.
fn from_catalog_metadata(entry: &serde_json::Value) -> Option<UpstreamProtocol> {
    let values = ["supported_endpoints", "endpoints", "supported_apis"]
        .iter()
        .find_map(|k| entry.get(*k))
        .and_then(|v| v.as_array())?;

    let mut seen = Vec::new();
    for v in values {
        let s = v.as_str()?.to_ascii_lowercase();
        if s.contains("/v1/messages") || s.contains("messages") {
            seen.push(UpstreamProtocol::AnthropicMessages);
        } else if s.contains("/v1/responses") || s.contains("responses") {
            seen.push(UpstreamProtocol::OpenAiResponses);
        } else if s.contains("chat/completions") || s.contains("chat_completions") {
            seen.push(UpstreamProtocol::OpenAiChat);
        } else if s.contains("generatecontent") {
            seen.push(UpstreamProtocol::GoogleGenerate);
        }
    }
    // Ambiguous metadata (several dialects advertised) is no better
    // than none — fall through to the probe, which finds out what
    // actually works rather than what's claimed.
    match seen.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// The smallest completion that still exercises the real code path:
/// one token out, one word in.
fn probe_request(upstream_model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: upstream_model.to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::Value::String("hi".to_string()),
            ..Default::default()
        }],
        temperature: None,
        max_tokens: Some(1),
        stream: None,
        extra: serde_json::Value::Null,
        caller_user_id: None,
        caller_user_email: None,
        trace_id: None,
    }
}

/// Does this failure mean "wrong dialect" (try the next candidate) or
/// "stop probing entirely"?
///
/// Auth and rate-limit failures say nothing about the dialect — every
/// candidate would fail the same way, so burning three requests to
/// learn that is pure waste. Everything else is treated as a dialect
/// rejection, which is the conservative reading: the worst case is one
/// extra probe request.
fn is_fatal_for_probing(err: &GatewayError) -> bool {
    match err {
        GatewayError::UpstreamAuthError | GatewayError::UpstreamRateLimited { .. } => true,
        GatewayError::ProviderHttpError { status, .. } => *status == 401 || *status == 403,
        _ => false,
    }
}

/// Resolve the protocol for every `upstream_model` in one import.
///
/// Returns a map from upstream model name to the protocol that answered.
/// Models absent from the map stay NULL — the runtime works them out on
/// first use.
pub(crate) async fn resolve_protocols(
    provider: &Provider,
    encryption_key: &str,
    upstream_models: &[String],
    // `catalog`: entries keyed by upstream model, when the caller has
    // them from `/v1/models`. Lets the free metadata path run before
    // any billable request.
    catalog: &HashMap<String, serde_json::Value>,
) -> HashMap<String, UpstreamProtocol> {
    let materials = ProviderMaterials::from_provider(provider, encryption_key);
    let mut resolved: HashMap<String, UpstreamProtocol> = HashMap::new();
    let mut by_family: HashMap<String, UpstreamProtocol> = HashMap::new();
    // One auth failure means every subsequent probe fails identically.
    let mut probing_disabled = false;

    for model in upstream_models {
        if let Some(entry) = catalog.get(model)
            && let Some(p) = from_catalog_metadata(entry)
        {
            resolved.insert(model.clone(), p);
            continue;
        }

        let key = family_key(model);
        if let Some(p) = by_family.get(&key) {
            resolved.insert(model.clone(), *p);
            continue;
        }

        let candidates = UpstreamProtocol::candidates_for(&materials.provider_type, model);
        // A provider whose transport admits no alternative needs no
        // probe: there is nothing to choose between.
        if let [only] = candidates.as_slice() {
            resolved.insert(model.clone(), *only);
            by_family.insert(key, *only);
            continue;
        }
        if probing_disabled {
            continue;
        }

        for candidate in candidates {
            let adapter = build_adapter(candidate, &materials);
            match adapter.chat_completion_boxed(probe_request(model)).await {
                Ok(_) => {
                    tracing::info!(
                        provider = %materials.name,
                        model = %model,
                        protocol = %candidate,
                        "Probed upstream protocol"
                    );
                    resolved.insert(model.clone(), candidate);
                    by_family.insert(key.clone(), candidate);
                    break;
                }
                Err(e) if is_fatal_for_probing(&e) => {
                    tracing::warn!(
                        provider = %materials.name,
                        model = %model,
                        "Protocol probe abandoned — upstream rejected our credentials: {e}"
                    );
                    probing_disabled = true;
                    break;
                }
                Err(e) => {
                    tracing::debug!(
                        provider = %materials.name,
                        model = %model,
                        protocol = %candidate,
                        "Protocol candidate rejected: {e}"
                    );
                }
            }
        }
    }

    resolved
}

/// Probe in the background and write the results back to the routes.
///
/// Import returns the moment the rows land — nobody waits on a page
/// while the gateway talks to an upstream N times. Requests that arrive
/// before this finishes are not broken by it either: an un-probed route
/// runs on the provider type's default dialect, and if that's wrong the
/// runtime relearn path fixes it on the first call. This task only
/// removes that first-call retry.
///
/// Fire-and-forget by design: a failure here costs one retry later, so
/// there is nothing worth propagating to the caller.
pub(crate) fn spawn_probe(
    state: &crate::app::AppState,
    provider: think_watch_common::models::Provider,
    upstream_models: Vec<String>,
) {
    if upstream_models.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let protocols = resolve_protocols(
            &provider,
            &state.config.encryption_key,
            &upstream_models,
            &HashMap::new(),
        )
        .await;
        if protocols.is_empty() {
            return;
        }

        let (models, values): (Vec<String>, Vec<String>) = protocols
            .into_iter()
            .map(|(m, p)| (m, p.as_str().to_string()))
            .unzip();
        // Only fill in routes nobody has decided on yet: between the
        // import and this task finishing, a live request may already
        // have relearned a dialect the hard way, and that observation
        // beats ours — it came from real traffic.
        let updated = sqlx::query(
            r#"UPDATE model_routes AS mr
                  SET upstream_protocol = t.protocol
                 FROM UNNEST($2::TEXT[], $3::TEXT[]) AS t(upstream, protocol)
                WHERE mr.provider_id = $1
                  AND mr.upstream_model = t.upstream
                  AND mr.upstream_protocol IS NULL"#,
        )
        .bind(provider.id)
        .bind(&models)
        .bind(&values)
        .execute(&state.db)
        .await;

        match updated {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::info!(
                    provider = %provider.name,
                    routes = r.rows_affected(),
                    "Upstream protocols probed — routes updated"
                );
                // Swap the new dialects into the live router so the
                // next request uses them without waiting for the next
                // provider/model edit to trigger a rebuild.
                crate::app::rebuild_gateway_router(&state).await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                provider = %provider.name,
                "Probed upstream protocols but could not persist them: {e}"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_key_groups_vendor_prefixed_and_bare_ids() {
        assert_eq!(family_key("anthropic.claude-opus-4"), "anthropic");
        assert_eq!(family_key("openai/gpt-4o"), "openai");
        assert_eq!(family_key("claude-3-5-sonnet"), "claude");
        assert_eq!(family_key("gpt-4o"), "gpt");
        // Same family ⇒ one probe covers both.
        assert_eq!(
            family_key("anthropic.claude-opus-4"),
            family_key("anthropic.claude-haiku-4")
        );
    }

    #[test]
    fn catalog_metadata_is_read_when_unambiguous() {
        let entry = serde_json::json!({"supported_endpoints": ["/v1/messages"]});
        assert_eq!(
            from_catalog_metadata(&entry),
            Some(UpstreamProtocol::AnthropicMessages)
        );
        let entry = serde_json::json!({"endpoints": ["/v1/responses"]});
        assert_eq!(
            from_catalog_metadata(&entry),
            Some(UpstreamProtocol::OpenAiResponses)
        );
    }

    #[test]
    fn ambiguous_or_missing_metadata_falls_through_to_probing() {
        // Two dialects advertised — believing either one over the other
        // would be a guess; the probe finds out what actually answers.
        let entry =
            serde_json::json!({"supported_endpoints": ["/v1/messages", "/v1/chat/completions"]});
        assert_eq!(from_catalog_metadata(&entry), None);
        assert_eq!(from_catalog_metadata(&serde_json::json!({"id": "m"})), None);
    }

    #[test]
    fn auth_failures_stop_probing_but_dialect_rejections_do_not() {
        assert!(is_fatal_for_probing(&GatewayError::UpstreamAuthError));
        assert!(is_fatal_for_probing(&GatewayError::ProviderHttpError {
            status: 403,
            message: "forbidden".into(),
        }));
        // "model does not support this API" — exactly what we want to
        // react to by trying the next candidate.
        assert!(!is_fatal_for_probing(&GatewayError::ProviderHttpError {
            status: 400,
            message: "does not support the '/v1/chat/completions' API".into(),
        }));
    }
}
