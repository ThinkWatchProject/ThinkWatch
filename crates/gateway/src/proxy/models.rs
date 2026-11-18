//! `GET /v1/models` and Gemini's `GET /v1beta/models` — the models this
//! gateway routes that the calling key may use, in the shape each kind of
//! client reads.

use std::collections::BTreeSet;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::{GatewayRequestIdentity, GatewayState, ModelDiscovery};
use crate::model_mapping::ModelMapper;
use crate::router::ModelRouter;

/// The models `identity` may call, in name order: each name the router
/// serves, and each entry of the caller's allow-list taken as an exact
/// model name — every one put through the alias mapping, the model check
/// and the route lookup a request naming it would meet.
///
/// Router names can be prefixes (a provider with no routes of its own is
/// served under `gpt-`, `claude-`, …), so a key narrowed to `gpt-4o` is
/// let through for `gpt-4o` although no router name is `gpt-4o`, and only
/// its allow-list names it. A listed model is one a request for it is let
/// through for and routed, and a model the key may not use is not listed.
fn callable_models(
    router: &ModelRouter,
    cfg: &ModelDiscovery,
    identity: &GatewayRequestIdentity,
) -> Vec<String> {
    let mut models = callable_models_in(router, &cfg.mapper(), identity);
    cfg.apply_visibility(&mut models, router, identity);
    models
}

fn callable_models_in(
    router: &ModelRouter,
    mapper: &ModelMapper,
    identity: &GatewayRequestIdentity,
) -> Vec<String> {
    let callable = |name: &str| {
        let mapped = mapper.map(name);
        identity.may_call(&mapped) && router.route(&mapped).is_some()
    };
    let mut models: BTreeSet<String> = router
        .list_models()
        .into_iter()
        .filter(|name| callable(name))
        .collect();
    if let Some(allowed) = &identity.allowed_models {
        models.extend(allowed.iter().filter(|name| callable(name)).cloned());
    }
    models.into_iter().collect()
}

/// GET /v1/models
///
/// Returns the models the calling key may use, in OpenAI-compatible
/// format.
pub async fn list_models_handler(
    State(state): State<GatewayState>,
    Extension(identity): Extension<GatewayRequestIdentity>,
) -> Json<serde_json::Value> {
    let cfg = ModelDiscovery::load(&state).await;
    let model_objects: Vec<serde_json::Value> =
        callable_models(&state.router.load(), &cfg, &identity)
            .into_iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "object": "model",
                    "created": 0,
                    "owned_by": "think-watch",
                })
            })
            .collect();

    Json(serde_json::json!({
        "object": "list",
        "data": model_objects,
    }))
}

/// GET /v1beta/models
///
/// The same list, in Gemini's shape: `name` carries the `models/` prefix
/// its clients strip.
pub async fn list_gemini_models_handler(
    State(state): State<GatewayState>,
    Extension(identity): Extension<GatewayRequestIdentity>,
) -> Json<serde_json::Value> {
    let cfg = ModelDiscovery::load(&state).await;
    let models: Vec<serde_json::Value> = callable_models(&state.router.load(), &cfg, &identity)
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "name": format!("models/{id}"),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    Json(serde_json::json!({ "models": models }))
}

#[derive(serde::Deserialize)]
pub struct CodexModelsQuery {
    client_version: Option<String>,
}

/// The native Codex model picker uses the same model permission as inference.
pub async fn list_codex_models_handler(
    State(state): State<GatewayState>,
    Extension(identity): Extension<GatewayRequestIdentity>,
    Query(query): Query<CodexModelsQuery>,
) -> Response {
    if query.client_version.as_ref().is_some_and(|version| {
        version.len() > 32
            || version.split('.').count() != 3
            || !version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }) {
        return (
            StatusCode::BAD_REQUEST,
            "Expected client_version in major.minor.patch format",
        )
            .into_response();
    }
    let cfg = ModelDiscovery::load(&state).await;
    let router = state.router.load_full();
    let names = callable_models(&router, &cfg, &identity);
    let catalog = super::model_discovery::codex_catalog(
        &router,
        &cfg,
        &names,
        &identity,
        query.client_version.as_deref(),
    )
    .await;
    let body = catalog.to_string();
    if body.len() > 1024 * 1024 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Model catalog exceeds one MiB",
        )
            .into_response();
    }
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::RouteEntry;
    use std::sync::Arc;
    use uuid::Uuid;

    fn entry() -> RouteEntry {
        RouteEntry {
            upstream: Arc::new(crate::proxy::transport::Upstream::new(
                "https://example.invalid",
                Vec::new(),
                crate::proxy::transport::Shape::Standard,
                "openai",
            )),
            provider_id: Uuid::nil(),
            route_id: Uuid::nil(),
            provider_name: "openai".into(),
            upstream_model: None,
            weight: 100,
            label: None,
            rpm_cap: None,
            tpm_cap: None,
            protocol: crate::protocol::UpstreamProtocol::OpenAiChat,
            alternates: Vec::new(),
        }
    }

    /// A provider served under prefixes only, as one with no routes of
    /// its own is, plus one exact route.
    fn router() -> ModelRouter {
        let mut router = ModelRouter::new();
        for name in ["gpt-", "o1-", "claude-sonnet-4-5"] {
            router.register_route(name, entry());
        }
        router
    }

    fn narrowed(allowed: Option<&[&str]>) -> GatewayRequestIdentity {
        GatewayRequestIdentity {
            allowed_models: allowed.map(|a| a.iter().map(|m| m.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn hiding_an_original_requires_the_same_access_as_its_alias() {
        let cfg = ModelDiscovery::parse(
            serde_json::json!({"aliases":{"code":{"target":"gpt-4o","hide_original":true}}}),
        )
        .unwrap();
        let router = router();
        let allowed = narrowed(Some(&["gpt-4o"]));
        assert_eq!(callable_models(&router, &cfg, &allowed), ["code"]);
        assert!(allowed.may_call(&cfg.resolve("gpt-4o")));
        assert!(callable_models(&router, &cfg, &narrowed(Some(&["code"]))).is_empty());
    }

    #[test]
    fn an_unrestricted_key_lists_what_the_router_serves() {
        assert_eq!(
            callable_models_in(&router(), &ModelMapper::new(), &narrowed(None)),
            ["claude-sonnet-4-5", "gpt-", "o1-"]
        );
    }

    #[test]
    fn a_key_narrowed_to_a_model_behind_a_prefix_route_lists_it() {
        assert_eq!(
            callable_models_in(
                &router(),
                &ModelMapper::new(),
                &narrowed(Some(&["gpt-4o", "o1-mini", "gemini-2.5-pro"]))
            ),
            // `gemini-2.5-pro` is allowed but nothing routes it.
            ["gpt-4o", "o1-mini"]
        );
    }

    #[test]
    fn a_name_both_routed_and_allowed_is_listed_once() {
        assert_eq!(
            callable_models_in(
                &router(),
                &ModelMapper::new(),
                &narrowed(Some(&["gpt-", "claude-sonnet-4-5"]))
            ),
            ["claude-sonnet-4-5", "gpt-"]
        );
    }

    #[test]
    fn an_allowed_name_is_listed_as_a_request_names_it() {
        // `fast` is an alias of `gpt-4o-mini`: a request for `fast` is
        // checked and routed as `gpt-4o-mini`.
        let mut mapper = ModelMapper::new();
        mapper.add_exact("fast", "gpt-4o-mini");
        mapper.add_exact("slow", "llama-3");
        assert_eq!(
            callable_models_in(&router(), &mapper, &narrowed(Some(&["fast", "slow"]))),
            Vec::<String>::new(),
            "`fast` maps to a model the key does not allow"
        );
        assert_eq!(
            callable_models_in(
                &router(),
                &mapper,
                &narrowed(Some(&["fast", "slow", "gpt-4o-mini", "llama-3"]))
            ),
            // `slow` maps to `llama-3`, which nothing routes.
            ["fast", "gpt-4o-mini"]
        );
    }

    #[test]
    fn an_empty_allow_list_lists_nothing() {
        assert!(
            callable_models_in(&router(), &ModelMapper::new(), &narrowed(Some(&[]))).is_empty()
        );
    }
}
