//! `GET /v1/models` and Gemini's `GET /v1beta/models` — the models this
//! gateway routes that the calling key may use, in the shape each kind of
//! client reads.

use axum::extract::State;
use axum::{Extension, Json};

use super::{GatewayRequestIdentity, GatewayState};

/// The routed models `identity` may call: each name the router serves,
/// put through the same alias mapping and model check a request for it
/// would meet. A listed model is one a request for it is let through for,
/// and a model the key may not use is not listed.
fn callable_models(state: &GatewayState, identity: &GatewayRequestIdentity) -> Vec<String> {
    state
        .router
        .load()
        .list_models()
        .into_iter()
        .filter(|id| identity.may_call(&state.model_mapper.map(id)))
        .collect()
}

/// GET /v1/models
///
/// Returns the models the calling key may use, in OpenAI-compatible
/// format.
pub async fn list_models_handler(
    State(state): State<GatewayState>,
    Extension(identity): Extension<GatewayRequestIdentity>,
) -> Json<serde_json::Value> {
    let model_objects: Vec<serde_json::Value> = callable_models(&state, &identity)
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
    let models: Vec<serde_json::Value> = callable_models(&state, &identity)
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
