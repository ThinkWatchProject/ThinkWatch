use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::dto::{CreateProviderRequest, ProviderHeader};
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;
use think_watch_common::validation::validate_url;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[utoipa::path(
    get,
    path = "/api/admin/providers",
    tag = "Providers",
    security(("bearer_token" = [])),
    responses(
        (status = 200, description = "List of AI providers"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn list_providers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Provider>>, AppError> {
    auth_user.require_permission("providers:read")?;
    auth_user
        .assert_scope_global(&state.db, "providers:read")
        .await?;
    let providers = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE deleted_at IS NULL ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(providers))
}

#[utoipa::path(
    post,
    path = "/api/admin/providers",
    tag = "Providers",
    security(("bearer_token" = [])),
    request_body(content_type = "application/json", description = "Provider creation request"),
    responses(
        (status = 200, description = "Provider created"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn create_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateProviderRequest>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:create")?;
    auth_user
        .assert_scope_global(&state.db, "providers:create")
        .await?;
    if req.name.is_empty() || req.base_url.is_empty() {
        return Err(AppError::BadRequest(
            "name and base_url are required".into(),
        ));
    }

    // SSRF prevention: validate base_url
    validate_url(&req.base_url)?;

    // Store unified headers in config_json
    let mut config = req.config.unwrap_or(serde_json::json!({}));
    config["headers"] = serde_json::to_value(&req.headers)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;

    let provider = sqlx::query_as::<_, Provider>(
        r#"INSERT INTO providers (name, display_name, provider_type, base_url, config_json)
           VALUES ($1, $2, $3, $4, $5) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.display_name)
    .bind(&req.provider_type)
    .bind(&req.base_url)
    .bind(&config)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("provider.created")
            .resource("provider")
            .resource_id(provider.id.to_string())
            .detail(serde_json::json!({ "name": &req.name })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(provider))
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateProviderRequest {
    pub display_name: Option<String>,
    pub base_url: Option<String>,
    /// Unified request headers (auth + custom + identity templates).
    pub headers: Option<Vec<ProviderHeader>>,
}

#[utoipa::path(
    patch,
    path = "/api/admin/providers/{id}",
    tag = "Providers",
    security(("bearer_token" = [])),
    params(
        ("id" = uuid::Uuid, Path, description = "Provider ID"),
    ),
    request_body = UpdateProviderRequest,
    responses(
        (status = 200, description = "Provider updated"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn update_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateProviderRequest>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:update")?;
    auth_user
        .assert_scope_global(&state.db, "providers:update")
        .await?;
    let existing = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    let display_name = req
        .display_name
        .as_deref()
        .unwrap_or(&existing.display_name);
    let base_url = req.base_url.as_deref().unwrap_or(&existing.base_url);

    if req.base_url.is_some() {
        validate_url(base_url)?;
    }

    // Update headers in config_json if provided
    let config_json = if let Some(ref headers) = req.headers {
        let mut config = existing.config_json.clone();
        config["headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
        config
    } else {
        existing.config_json.clone()
    };

    let updated = sqlx::query_as::<_, Provider>(
        r#"UPDATE providers SET display_name = $2, base_url = $3, config_json = $4
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(base_url)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("provider.updated")
            .resource("provider")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.name })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(updated))
}

#[utoipa::path(
    get,
    path = "/api/admin/providers/{id}",
    tag = "Providers",
    security(("bearer_token" = [])),
    params(
        ("id" = uuid::Uuid, Path, description = "Provider ID"),
    ),
    responses(
        (status = 200, description = "Provider details"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn get_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:read")?;
    auth_user
        .assert_scope_global(&state.db, "providers:read")
        .await?;
    let provider = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    Ok(Json(provider))
}

#[utoipa::path(
    delete,
    path = "/api/admin/providers/{id}",
    tag = "Providers",
    security(("bearer_token" = [])),
    params(
        ("id" = uuid::Uuid, Path, description = "Provider ID"),
    ),
    responses(
        (status = 200, description = "Provider deleted"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn delete_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("providers:delete")?;
    auth_user
        .assert_scope_global(&state.db, "providers:delete")
        .await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM providers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;

    // Soft-delete + drop routes in one transaction. The `model_routes`
    // FK is `ON DELETE CASCADE`, but since we only flip `deleted_at`
    // the cascade doesn't fire — hence the explicit DELETE below.
    // Orphaned routes would otherwise show up in the Models page with
    // a raw provider UUID and no way to edit them.
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let routes_deleted = sqlx::query("DELETE FROM model_routes WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;

    state.audit.log(
        auth_user
            .audit("provider.deleted")
            .resource("provider")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name, "routes_deleted": routes_deleted })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ---------------------------------------------------------------------------
// Test connection — used by the setup wizard and Add Provider dialog so
// admins can verify base URL + API key + custom headers without persisting.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct TestProviderRequest {
    pub provider_type: String,
    pub base_url: String,
    /// Unified request headers (auth + custom).
    #[serde(default)]
    pub headers: Vec<ProviderHeader>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TestProviderResponse {
    pub success: bool,
    pub message: String,
    /// HTTP status code returned by upstream, if a response was received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Number of models returned by the upstream `/v1/models` (where applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_count: Option<usize>,
    /// Model IDs returned by the upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

/// Authenticated route — used by the providers admin page.
#[utoipa::path(
    post,
    path = "/api/admin/providers/test",
    tag = "Providers",
    security(("bearer_token" = [])),
    request_body = TestProviderRequest,
    responses(
        (status = 200, description = "Connection test result", body = TestProviderResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn test_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, AppError> {
    auth_user.require_permission("providers:create")?;
    auth_user
        .assert_scope_global(&state.db, "providers:create")
        .await?;
    run_provider_test(req).await
}

/// Unauthenticated route — used by the setup wizard before any user exists.
/// Gated by an extra check that setup is not yet complete, so anonymous
/// callers can't probe arbitrary URLs against an installed instance.
pub async fn test_provider_unauthenticated(
    State(state): State<AppState>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, AppError> {
    if state.dynamic_config.is_initialized().await {
        return Err(AppError::Forbidden(
            "Setup already completed — use the authenticated endpoint".into(),
        ));
    }
    run_provider_test(req).await
}

pub(crate) async fn run_provider_test(
    req: TestProviderRequest,
) -> Result<Json<TestProviderResponse>, AppError> {
    if req.base_url.is_empty() {
        return Err(AppError::BadRequest("base_url is required".into()));
    }
    validate_url(&req.base_url)?;

    // Provider-specific probe URL. We always hit a cheap, read-only
    // endpoint that requires auth so a wrong key is detected too.
    let url = match req.provider_type.as_str() {
        "anthropic" => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
        "google" => format!("{}/v1beta/models", req.base_url.trim_end_matches('/')),
        // openai / azure / custom — all OpenAI-compatible /v1/models
        _ => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to build HTTP client: {e}")))?;

    let mut builder = client.get(&url);
    // Apply all headers directly — auth is now part of the unified headers list
    for h in &req.headers {
        builder = builder.header(&h.key, &h.value);
    }

    let started = std::time::Instant::now();
    let result = builder.send().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            if status.is_success() {
                // Extract model list from standard shapes:
                // OpenAI/Anthropic: { "data": [{ "id": "..." }, ...] }
                // Google:           { "models": [{ "name": "models/..." }, ...] }
                let models_array = body
                    .get("data")
                    .and_then(|v| v.as_array())
                    .or_else(|| body.get("models").and_then(|v| v.as_array()));

                let (model_count, models) = if let Some(arr) = models_array {
                    let ids: Vec<String> = arr
                        .iter()
                        .filter_map(|m| {
                            m.get("id")
                                .or_else(|| m.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect();
                    (Some(ids.len()), Some(ids))
                } else {
                    (None, None)
                };

                Ok(Json(TestProviderResponse {
                    success: true,
                    message: match model_count {
                        Some(n) => format!("Connected successfully — {n} models available"),
                        None => "Connected successfully".to_string(),
                    },
                    status_code: Some(status.as_u16()),
                    latency_ms,
                    model_count,
                    models,
                }))
            } else {
                let upstream_err = body
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
                Ok(Json(TestProviderResponse {
                    success: false,
                    message: format!("HTTP {}: {upstream_err}", status.as_u16()),
                    status_code: Some(status.as_u16()),
                    latency_ms,
                    model_count: None,
                    models: None,
                }))
            }
        }
        Err(e) => Ok(Json(TestProviderResponse {
            success: false,
            message: format!("Request failed: {e}"),
            status_code: None,
            latency_ms,
            model_count: None,
            models: None,
        })),
    }
}
