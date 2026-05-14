use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::{CreateProviderRequest, ProviderHeader};
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;
use think_watch_common::validation::validate_url;

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
// Read paths (`load_providers_into_router`) detect the wrapper and decrypt.
// Plaintext legacy rows still load — they emit a one-shot `tracing::warn!` so
// admins know to re-save them. A startup backfill (`backfill_provider_secrets`)
// converts every plaintext row on first boot, after which no plaintext exists.
// ---------------------------------------------------------------------------

/// JSON marker key used to distinguish encrypted-at-rest payloads from
/// legacy plaintext strings inside `providers.config_json`.
pub(crate) const ENC_MARKER: &str = "$enc";

/// Encrypt `plaintext` with the master encryption key and return a
/// `{"$enc": "<hex-envelope>"}` JSON value suitable for storing in
/// `config_json`. Empty inputs round-trip unchanged: callers shouldn't
/// burn AES on `""`, and the loader treats missing/empty fields as
/// "no credential supplied" anyway.
pub(crate) fn encrypt_secret_to_json(
    plaintext: &str,
    encryption_key: &str,
) -> Result<serde_json::Value, AppError> {
    if plaintext.is_empty() {
        return Ok(serde_json::Value::String(String::new()));
    }
    let key = crypto::parse_encryption_key(encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
    let bytes = crypto::encrypt(plaintext.as_bytes(), &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Provider secret encrypt failed: {e}")))?;
    let encoded = hex::encode(bytes);
    Ok(serde_json::json!({ ENC_MARKER: encoded }))
}

/// Inverse of [`encrypt_secret_to_json`]. Recognises three shapes:
///   - `{"$enc": "<hex>"}` → decrypt envelope (production read path)
///   - `"<plaintext>"` → legacy plaintext row (returned verbatim;
///     caller logs a warn so admins re-save)
///   - missing / non-string / not-an-object → empty string
///
/// Returns `(plaintext, was_encrypted)` so the caller can tell whether a
/// `tracing::warn!` is warranted.
pub(crate) fn decrypt_secret_from_json(
    value: &serde_json::Value,
    encryption_key: &str,
) -> Result<(String, bool), AppError> {
    // Encrypted envelope?
    if let Some(obj) = value.as_object()
        && let Some(hex_str) = obj.get(ENC_MARKER).and_then(|v| v.as_str())
    {
        let bytes = hex::decode(hex_str).map_err(|e| {
            AppError::Internal(anyhow::anyhow!("Provider secret hex decode failed: {e}"))
        })?;
        let key = crypto::parse_encryption_key(encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        let plain = crypto::decrypt(&bytes, &key).map_err(|e| {
            AppError::Internal(anyhow::anyhow!("Provider secret decrypt failed: {e}"))
        })?;
        let s = String::from_utf8(plain).map_err(|e| {
            AppError::Internal(anyhow::anyhow!("Provider secret is not valid UTF-8: {e}"))
        })?;
        return Ok((s, true));
    }
    // Legacy plaintext or empty.
    Ok((value.as_str().unwrap_or("").to_string(), false))
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

/// If `config["aws_secret_access_key"]` is a plaintext string, wrap it
/// with the encrypted envelope. Already-encrypted (`{"$enc": ...}`) or
/// empty values are left alone.
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
    // Already encrypted — preserve as-is.
    if raw.is_object() && raw.as_object().is_some_and(|o| o.contains_key(ENC_MARKER)) {
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

    // Store unified headers in config_json, encrypting every header
    // value at rest. AWS bedrock secrets (when nested in `config`) get
    // the same wrapper.
    let mut config = req.config.unwrap_or(serde_json::json!({}));
    encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
    config["headers"] = encrypt_headers_for_storage(&req.headers, &state.config.encryption_key)?;

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

    // Update headers in config_json if provided. Encrypt every header
    // value at rest; if any AWS secret rode along in config_json we
    // re-wrap it too (handles admins editing plaintext legacy rows).
    let config_json = if let Some(ref headers) = req.headers {
        let mut config = existing.config_json.clone();
        encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
        config["headers"] = encrypt_headers_for_storage(headers, &state.config.encryption_key)?;
        config
    } else {
        let mut config = existing.config_json.clone();
        encrypt_aws_secret_in_config(&mut config, &state.config.encryption_key)?;
        config
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
        let obj = json
            .as_object()
            .expect("encrypted secret must be a JSON object");
        assert!(
            obj.contains_key(ENC_MARKER),
            "wrapper must include the $enc marker"
        );
        let hex_str = obj[ENC_MARKER].as_str().unwrap();
        assert!(
            !hex_str.contains("sk-supersecret"),
            "plaintext must not appear in hex envelope: {hex_str}"
        );

        let (plain, was_encrypted) = decrypt_secret_from_json(&json, test_hex_key()).unwrap();
        assert!(was_encrypted, "round-tripped value reports as encrypted");
        assert_eq!(plain, "sk-supersecret");
    }

    #[test]
    fn empty_plaintext_stays_empty_no_envelope() {
        // Don't burn AES on "" — the loader treats missing/empty as
        // "no credential supplied".
        let json = encrypt_secret_to_json("", test_hex_key()).unwrap();
        assert_eq!(json, serde_json::Value::String(String::new()));
        let (plain, was_encrypted) = decrypt_secret_from_json(&json, test_hex_key()).unwrap();
        assert!(!was_encrypted);
        assert_eq!(plain, "");
    }

    #[test]
    fn legacy_plaintext_decodes_with_flag() {
        // Pre-encryption dev DBs still hold plain strings. The decoder
        // returns them verbatim and flags `was_encrypted = false`
        // so the loader can emit a one-shot re-save warn.
        let legacy = serde_json::Value::String("Bearer abc123".to_string());
        let (plain, was_encrypted) = decrypt_secret_from_json(&legacy, test_hex_key()).unwrap();
        assert!(
            !was_encrypted,
            "plaintext string must NOT report as encrypted"
        );
        assert_eq!(plain, "Bearer abc123");
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
                v.is_object() && v.as_object().unwrap().contains_key(ENC_MARKER),
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
            wrapped.is_object() && wrapped.as_object().unwrap().contains_key(ENC_MARKER),
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
