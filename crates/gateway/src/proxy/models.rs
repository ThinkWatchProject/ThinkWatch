//! `GET /v1/models` and Gemini's `GET /v1beta/models` — the models this
//! gateway routes, in the shape each kind of client reads.

use axum::Json;
use axum::extract::State;

use super::GatewayState;

/// GET /v1/models
///
/// Returns the list of available models in OpenAI-compatible format.
pub async fn list_models_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let models = state.router.load().list_models();

    let model_objects: Vec<serde_json::Value> = models
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
) -> Json<serde_json::Value> {
    let models: Vec<serde_json::Value> = state
        .router
        .load()
        .list_models()
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
