use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::dto::{CreateProviderRequest, ProviderHeader};
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ---------------------------------------------------------------------------
// At-rest encryption for provider secrets stored in `providers.config_json`.
//
// Every header `value` and the `aws_secret_access_key` field are wrapped as
// `{"$enc": "<hex-envelope>"}` before INSERT/UPDATE. The hex payload is the
// AES-256-GCM versioned envelope produced by `think_watch_common::crypto`
// (same envelope MCP OAuth client_secrets use). Hex (not base64) keeps us
// dependency-aligned with the OIDC / TOTP storage path which already encodes
// the envelope as hex.
//
// ---------------------------------------------------------------------------

use think_watch_common::json_secret::JsonSecret;

/// Encrypt `plaintext` and return a value suitable for storing inside
/// `providers.config_json`. Thin wrapper over [`JsonSecret::encrypt`]
/// that exposes the unified envelope shape to callers in this module.
pub(crate) fn encrypt_secret_to_json(
    plaintext: &str,
    encryption_key: &str,
) -> Result<serde_json::Value, AppError> {
    Ok(JsonSecret::encrypt(plaintext, encryption_key)?.to_json())
}

/// Inverse of [`encrypt_secret_to_json`]. Returns the plaintext or an
/// error if the wire value isn't a valid envelope (corrupted /
/// hand-edited row).
pub(crate) fn decrypt_secret_from_json(
    value: &serde_json::Value,
    encryption_key: &str,
) -> Result<String, AppError> {
    JsonSecret::from_json(value)?.decrypt(encryption_key)
}

/// Take a header list as supplied in a request and return a JSON array
/// whose `value` fields are encrypted-at-rest. Headers with empty values
/// remain plaintext-empty (no point allocating a ciphertext for `""`).
fn encrypt_headers_for_storage(
    headers: &[ProviderHeader],
    encryption_key: &str,
) -> Result<serde_json::Value, AppError> {
    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let enc_value = encrypt_secret_to_json(&h.value, encryption_key)?;
        out.push(serde_json::json!({
            "key": h.key,
            "value": enc_value,
        }));
    }
    Ok(serde_json::Value::Array(out))
}

/// Decrypt the header list stored in a provider's `config_json`.
///
/// The stored `value` is a `{"$enc": …}` envelope, so deserializing the
/// array straight into `Vec<ProviderHeader>` (whose `value` is a
/// `String`) always fails — callers that did that silently ended up
/// with zero headers and made unauthenticated upstream calls. Headers
/// that fail to decrypt are skipped with a loud log line: the row is
/// corrupted, and forwarding a ciphertext as a header value would be
/// worse than omitting it.
pub(crate) fn decrypt_headers_from_config(
    config_json: &serde_json::Value,
    encryption_key: &str,
    provider_name: &str,
) -> Vec<ProviderHeader> {
    config_json
        .get("headers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let key = item.get("key")?.as_str()?.to_string();
                    let raw = item.get("value")?;
                    match decrypt_secret_from_json(raw, encryption_key) {
                        Ok(value) => Some(ProviderHeader { key, value }),
                        Err(e) => {
                            tracing::error!(
                                provider = %provider_name,
                                header = %key,
                                "Failed to decrypt provider header — skipping: {e}"
                            );
                            None
                        }
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Same as [`encrypt_headers_for_storage`], but a header submitted with
/// an empty value keeps whatever ciphertext is already stored under that
/// key. The read endpoints redact secrets (see
/// [`redact_provider_secrets`]), so the edit form can only ever send
/// back a blank for an untouched header — without this merge, opening
/// the dialog and pressing Save would silently wipe every API key.
/// Clearing a value is done by removing the header row, not by blanking
/// it.
fn merge_headers_for_storage(
    headers: &[ProviderHeader],
    existing_config: &serde_json::Value,
    encryption_key: &str,
) -> Result<serde_json::Value, AppError> {
    let stored = existing_config
        .get("headers")
        .and_then(|h| h.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let enc_value = if h.value.is_empty() {
            stored
                .iter()
                .find(|s| s.get("key").and_then(|k| k.as_str()) == Some(h.key.as_str()))
                .map(|s| s.get("value").cloned().unwrap_or(serde_json::Value::Null))
                .filter(JsonSecret::json_is_encrypted)
                .map_or_else(
                    || encrypt_secret_to_json(&h.value, encryption_key),
                    Ok::<_, AppError>,
                )?
        } else {
            encrypt_secret_to_json(&h.value, encryption_key)?
        };
        out.push(serde_json::json!({
            "key": h.key,
            "value": enc_value,
        }));
    }
    Ok(serde_json::Value::Array(out))
}

/// Strip at-rest ciphertext from a provider before it goes over the
/// wire. The read endpoints return `config_json` verbatim, so without
/// this the `{"$enc": …}` envelopes reach the browser — the admin UI
/// rendered one as the literal string `[object Object]` in the
/// header-value input and would have written that back as the new
/// secret on save.
///
/// Header entries gain an `encrypted` flag so the UI can tell "a secret
/// is stored, leave blank to keep it" from "genuinely empty".
fn redact_provider_secrets(provider: &mut Provider) {
    let Some(obj) = provider.config_json.as_object_mut() else {
        return;
    };
    if let Some(secret) = obj.get_mut("aws_secret_access_key")
        && JsonSecret::json_is_encrypted(secret)
    {
        *secret = serde_json::Value::String(String::new());
    }
    let Some(headers) = obj.get_mut("headers").and_then(|h| h.as_array_mut()) else {
        return;
    };
    for header in headers.iter_mut() {
        let Some(entry) = header.as_object_mut() else {
            continue;
        };
        let encrypted = entry
            .get("value")
            .is_some_and(JsonSecret::json_is_encrypted);
        entry.insert("value".into(), serde_json::Value::String(String::new()));
        entry.insert("encrypted".into(), serde_json::Value::Bool(encrypted));
    }
}

/// If `config["aws_secret_access_key"]` is a plaintext string, wrap it
/// with the encryption envelope. Already-encrypted or empty values are
/// left alone (idempotent under repeated calls).
fn encrypt_aws_secret_in_config(
    config: &mut serde_json::Value,
    encryption_key: &str,
) -> Result<(), AppError> {
    let Some(obj) = config.as_object_mut() else {
        return Ok(());
    };
    let Some(raw) = obj.get("aws_secret_access_key") else {
        return Ok(());
    };
    if JsonSecret::json_is_encrypted(raw) {
        return Ok(());
    }
    let Some(s) = raw.as_str() else {
        return Ok(());
    };
    if s.is_empty() {
        return Ok(());
    }
    let enc = encrypt_secret_to_json(s, encryption_key)?;
    obj.insert("aws_secret_access_key".to_string(), enc);
    Ok(())
}

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
    auth_user
        .require_global_permission(&state.db, "providers:read")
        .await?;
    let mut providers = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE deleted_at IS NULL ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;
    providers.iter_mut().for_each(redact_provider_secrets);

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
    auth_user
        .require_global_permission(&state.db, "providers:create")
        .await?;
    if req.name.is_empty() || req.base_url.is_empty() {
        return Err(AppError::BadRequest(
            "name and base_url are required".into(),
        ));
    }

    // SSRF prevention: validate base_url
    (state.url_validator)(&req.base_url)?;

    // Store unified headers in config_json, encrypting every header
    // value at rest. AWS bedrock secrets (when nested in `config`) get
    // the same wrapper.
    let mut config = req.config.unwrap_or(serde_json::json!({}));
    encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
    config["headers"] = encrypt_headers_for_storage(&req.headers, &state.config.encryption_key)?;

    let mut provider = sqlx::query_as::<_, Provider>(
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

    redact_provider_secrets(&mut provider);
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
    auth_user
        .require_global_permission(&state.db, "providers:update")
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
        (state.url_validator)(base_url)?;
    }

    // Update headers in config_json if provided. Encrypt every header
    // value at rest, keeping the stored ciphertext for headers submitted
    // blank (the read path redacts them, so blank = "unchanged"); if any
    // AWS secret rode along in config_json we re-wrap it too (handles
    // admins editing plaintext legacy rows).
    let config_json = if let Some(ref headers) = req.headers {
        let mut config = existing.config_json.clone();
        encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
        config["headers"] = merge_headers_for_storage(
            headers,
            &existing.config_json,
            &state.config.encryption_key,
        )?;
        config
    } else {
        let mut config = existing.config_json.clone();
        encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
        config
    };

    let mut updated = sqlx::query_as::<_, Provider>(
        r#"UPDATE providers SET display_name = $2, base_url = $3, config_json = $4
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(base_url)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await?;

    // A new base URL or credential can mean an entirely different
    // upstream, so every dialect we learned for this provider's routes
    // is now a guess about a host that may no longer be there. Clear
    // them and let the runtime relearn on first use — stale beats
    // wrong, and the relearn is invisible to the caller.
    if req.base_url.is_some() || req.headers.is_some() {
        let cleared: u64 = sqlx::query(
            "UPDATE model_routes SET upstream_protocol = NULL
             WHERE provider_id = $1 AND upstream_protocol IS NOT NULL",
        )
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
        // Same reasoning for the probe cache: "this upstream refuses
        // model X" described the old endpoint. Dropping it is also the
        // path back for an operator who fixed access upstream and
        // re-saved their credentials.
        let forgotten = crate::protocol_probe::clear_for_provider(&state.db, id).await;
        if cleared > 0 || forgotten > 0 {
            tracing::info!(
                provider = %existing.name,
                routes = cleared,
                probes = forgotten,
                "Provider endpoint changed — cleared learned protocols and probe verdicts"
            );
        }
    }

    state.audit.log(
        auth_user
            .audit("provider.updated")
            .resource("provider")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.name })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    redact_provider_secrets(&mut updated);
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
    auth_user
        .require_global_permission(&state.db, "providers:read")
        .await?;
    let mut provider = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;
    redact_provider_secrets(&mut provider);

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
    auth_user
        .require_global_permission(&state.db, "providers:delete")
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
    /// Existing provider the test is being run against, if any. Its
    /// stored secrets fill in any header submitted with an empty value —
    /// the edit dialog never receives the real values back (they're
    /// redacted), so without this "Test connection" from that dialog
    /// would always hit upstream unauthenticated.
    #[serde(default)]
    pub provider_id: Option<Uuid>,
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
    auth_user
        .require_global_permission(&state.db, "providers:create")
        .await?;

    let mut req = req;
    if let Some(provider_id) = req.provider_id {
        let provider = sqlx::query_as::<_, Provider>(
            "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(provider_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("Provider not found".into()))?;
        let stored = decrypt_headers_from_config(
            &provider.config_json,
            &state.config.encryption_key,
            &provider.name,
        );
        for header in req.headers.iter_mut().filter(|h| h.value.is_empty()) {
            if let Some(s) = stored.iter().find(|s| s.key == header.key) {
                header.value.clone_from(&s.value);
            }
        }
    }

    let http_client = (**state.http_client.load()).clone();
    run_provider_test(req, http_client, &state.url_validator).await
}

pub(crate) async fn run_provider_test(
    req: TestProviderRequest,
    client: reqwest::Client,
    // The pluggable SSRF guard from `AppState`, not the global
    // `validate_url`: this path fetches an admin-supplied URL exactly
    // like the gateway does, so it has to honour the same swappable
    // policy — otherwise the probe paths built on it can't be
    // integration-tested against a loopback mock at all.
    validate: &think_watch_common::validation::UrlValidator,
) -> Result<Json<TestProviderResponse>, AppError> {
    if req.base_url.is_empty() {
        return Err(AppError::BadRequest("base_url is required".into()));
    }
    validate(&req.base_url)?;

    // Provider-specific probe URL. We always hit a cheap, read-only
    // endpoint that requires auth so a wrong key is detected too.
    let url = match req.provider_type.as_str() {
        "anthropic" => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
        "google" => format!("{}/v1beta/models", req.base_url.trim_end_matches('/')),
        // openai / azure / custom — all OpenAI-compatible /v1/models
        _ => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
    };

    // `client` is now passed in from `test_provider` — uses the
    // shared http_client so this endpoint inherits the central
    // `redirect::Policy::none()` SSRF defense and the
    // `perf.http_client_secs` timeout knob — building a fresh
    // client here used to bypass both.

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hex_key() -> &'static str {
        // 32 bytes of zeroes → valid 64-char hex key for AES-256.
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }

    #[test]
    fn encrypt_secret_round_trips() {
        let json = encrypt_secret_to_json("sk-supersecret", test_hex_key()).unwrap();
        assert!(
            JsonSecret::json_is_encrypted(&json),
            "wrapper must round-trip as the encrypted envelope"
        );
        assert!(
            !serde_json::to_string(&json)
                .unwrap()
                .contains("sk-supersecret"),
            "plaintext must not appear in stored JSON"
        );

        let plain = decrypt_secret_from_json(&json, test_hex_key()).unwrap();
        assert_eq!(plain, "sk-supersecret");
    }

    #[test]
    fn empty_plaintext_stays_empty_no_envelope() {
        // Don't burn AES on "" — the loader treats missing/empty as
        // "no credential supplied".
        let json = encrypt_secret_to_json("", test_hex_key()).unwrap();
        assert_eq!(json, serde_json::Value::String(String::new()));
        let plain = decrypt_secret_from_json(&json, test_hex_key()).unwrap();
        assert_eq!(plain, "");
    }

    #[test]
    fn bare_string_at_rest_is_rejected() {
        // No producer writes a bare non-empty string into a secret
        // slot — encountering one at read time means the row was
        // corrupted or hand-edited. Surface the error instead of
        // silently degrading to a "no credentials" downstream
        // failure.
        let bare = serde_json::Value::String("Bearer abc123".to_string());
        assert!(decrypt_secret_from_json(&bare, test_hex_key()).is_err());
    }

    #[test]
    fn encrypt_headers_wraps_every_value() {
        let headers = vec![
            ProviderHeader {
                key: "Authorization".into(),
                value: "Bearer xyz".into(),
            },
            ProviderHeader {
                key: "X-Custom".into(),
                value: "not-a-secret-but-still-encrypted".into(),
            },
        ];
        let arr = encrypt_headers_for_storage(&headers, test_hex_key()).unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for item in arr {
            let v = &item["value"];
            assert!(
                JsonSecret::json_is_encrypted(v),
                "every header value must be wrapped in $enc — got {v}"
            );
            assert!(
                !serde_json::to_string(v).unwrap().contains("Bearer xyz"),
                "plaintext bearer leaked"
            );
        }
    }

    #[test]
    fn encrypt_aws_secret_wraps_only_plaintext() {
        let key = test_hex_key();
        let mut cfg = serde_json::json!({
            "aws_access_key_id": "AKIA-not-sensitive",
            "aws_secret_access_key": "secretsecret",
        });
        encrypt_aws_secret_in_config(&mut cfg, key).unwrap();
        let wrapped = &cfg["aws_secret_access_key"];
        assert!(
            JsonSecret::json_is_encrypted(wrapped),
            "plaintext aws_secret_access_key must be wrapped"
        );
        assert!(
            !serde_json::to_string(&cfg)
                .unwrap()
                .contains("secretsecret"),
            "plaintext aws secret leaked into stored JSON"
        );
        // Access key id is identifier, not a secret — left alone.
        assert_eq!(cfg["aws_access_key_id"], "AKIA-not-sensitive");

        // Idempotent: a second call doesn't double-encrypt.
        let first = cfg["aws_secret_access_key"].clone();
        encrypt_aws_secret_in_config(&mut cfg, key).unwrap();
        assert_eq!(cfg["aws_secret_access_key"], first);
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let json = encrypt_secret_to_json("topsecret", test_hex_key()).unwrap();
        let other = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let err = decrypt_secret_from_json(&json, other).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("decrypt"),
            "expected decrypt failure, got: {msg}"
        );
    }
}

#[cfg(test)]
mod header_plumbing_tests {
    use super::*;

    fn test_hex_key() -> &'static str {
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }

    fn config_with_header(key: &str, value: &str) -> serde_json::Value {
        let headers = encrypt_headers_for_storage(
            &[ProviderHeader {
                key: key.to_string(),
                value: value.to_string(),
            }],
            test_hex_key(),
        )
        .unwrap();
        serde_json::json!({ "headers": headers })
    }

    #[test]
    fn stored_headers_decrypt_back_to_plaintext() {
        // Regression: `serde_json::from_value::<Vec<ProviderHeader>>`
        // over the stored array can never succeed — `value` is a
        // `{"$enc": …}` object, not a String — so the remote-model
        // probe silently ran with zero headers and got a 401 from
        // upstream. The decrypt path must hand back the real value.
        let config = config_with_header("x-api-key", "sk-remote-probe");
        let headers = decrypt_headers_from_config(&config, test_hex_key(), "p");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].key, "x-api-key");
        assert_eq!(headers[0].value, "sk-remote-probe");
    }

    #[test]
    fn blank_header_patch_keeps_stored_secret() {
        let config = config_with_header("x-api-key", "sk-keep-me");
        let merged = merge_headers_for_storage(
            &[ProviderHeader {
                key: "x-api-key".to_string(),
                value: String::new(),
            }],
            &config,
            test_hex_key(),
        )
        .unwrap();
        assert_eq!(merged, config["headers"], "blank must reuse the ciphertext");
    }

    #[test]
    fn non_blank_header_patch_overwrites_stored_secret() {
        let config = config_with_header("x-api-key", "sk-old");
        let merged = merge_headers_for_storage(
            &[ProviderHeader {
                key: "x-api-key".to_string(),
                value: "sk-new".to_string(),
            }],
            &config,
            test_hex_key(),
        )
        .unwrap();
        let decrypted = decrypt_headers_from_config(
            &serde_json::json!({ "headers": merged }),
            test_hex_key(),
            "p",
        );
        assert_eq!(decrypted[0].value, "sk-new");
    }
}
