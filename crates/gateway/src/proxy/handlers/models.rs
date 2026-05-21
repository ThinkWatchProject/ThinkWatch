//! `GET /v1/models` — list available models in OpenAI-compatible format.

use axum::Json;
use axum::extract::State;

use super::super::GatewayState;

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
