use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::errors::AppError;
use think_watch_common::models::{McpServer, McpStoreTemplate};

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StoreListQuery {
    pub category: Option<String>,
    pub search: Option<String>,
    pub featured: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct StoreTemplateResponse {
    #[serde(flatten)]
    pub template: McpStoreTemplate,
    pub installed: bool,
}

#[derive(Debug, Serialize)]
pub struct CategoryCount {
    pub category: String,
    pub count: i64,
}

#[derive(Debug, Deserialize)]
pub struct InstallTemplateRequest {
    pub endpoint_url: Option<String>,
    pub auth_secret: Option<String>,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// GET /api/mcp/store — list templates
// ---------------------------------------------------------------------------

pub async fn list_templates(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<StoreListQuery>,
) -> Result<Json<Vec<StoreTemplateResponse>>, AppError> {
    // Fetch all installed template IDs for this instance
    let installed_ids: Vec<Uuid> = sqlx::query_scalar("SELECT template_id FROM mcp_store_installs")
        .fetch_all(&state.db)
        .await?;

    let installed_set: std::collections::HashSet<Uuid> = installed_ids.into_iter().collect();

    // Fetch all templates and filter in Rust — the store catalog is small
    // enough that dynamic SQL bind complexity isn't worth it.
    let templates = sqlx::query_as::<_, McpStoreTemplate>(
        "SELECT * FROM mcp_store_templates ORDER BY featured DESC, install_count DESC, name ASC",
    )
    .fetch_all(&state.db)
    .await?;

    let results: Vec<StoreTemplateResponse> = templates
        .into_iter()
        .filter(|t| {
            if let Some(ref cat) = q.category
                && t.category.as_deref() != Some(cat.as_str())
            {
                return false;
            }
            if let Some(ref search) = q.search {
                let s = search.to_lowercase();
                let name_match = t.name.to_lowercase().contains(&s);
                let desc_match = t
                    .description
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(&s))
                    .unwrap_or(false);
                let tag_match = t.tags.iter().any(|tag| tag.to_lowercase().contains(&s));
                if !name_match && !desc_match && !tag_match {
                    return false;
                }
            }
            if q.featured == Some(true) && !t.featured {
                return false;
            }
            true
        })
        .map(|t| {
            let installed = installed_set.contains(&t.id);
            StoreTemplateResponse {
                template: t,
                installed,
            }
        })
        .collect();

    Ok(Json(results))
}

// ---------------------------------------------------------------------------
// GET /api/mcp/store/categories
// ---------------------------------------------------------------------------

pub async fn list_categories(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<CategoryCount>>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        category: Option<String>,
        count: Option<i64>,
    }

    let rows = sqlx::query_as::<_, Row>(
        "SELECT category, COUNT(*) as count FROM mcp_store_templates GROUP BY category ORDER BY count DESC",
    )
    .fetch_all(&state.db)
    .await?;

    let categories = rows
        .into_iter()
        .filter_map(|r| {
            Some(CategoryCount {
                category: r.category?,
                count: r.count.unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(categories))
}

// ---------------------------------------------------------------------------
// POST /api/mcp/store/{slug}/install
// ---------------------------------------------------------------------------

pub async fn install_template(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<InstallTemplateRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    // a. Load template by slug
    let template =
        sqlx::query_as::<_, McpStoreTemplate>("SELECT * FROM mcp_store_templates WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Template '{slug}' not found")))?;

    // b. Determine endpoint
    let endpoint_url = match (&req.endpoint_url, &template.endpoint_template) {
        (Some(url), _) if !url.is_empty() => url.clone(),
        (_, Some(tmpl)) if !tmpl.is_empty() => tmpl.clone(),
        _ => {
            return Err(AppError::BadRequest(
                "endpoint_url is required for this template".into(),
            ));
        }
    };

    super::providers::validate_url(&endpoint_url)?;

    // Encrypt auth secret if provided
    let auth_encrypted = if let Some(ref secret) = req.auth_secret {
        let key = crypto::parse_encryption_key(&state.config.encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        Some(
            crypto::encrypt(secret.as_bytes(), &key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?,
        )
    } else {
        None
    };

    // Build config_json
    let config_json = {
        let mut config = serde_json::json!({});
        if let Some(ref headers) = req.custom_headers {
            super::providers::validate_custom_headers(headers)?;
            config["custom_headers"] = serde_json::to_value(headers).unwrap_or_default();
        }
        config
    };

    // c. Create mcp_servers row
    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (name, description, endpoint_url, transport_type, auth_type, auth_secret_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *"#,
    )
    .bind(&template.name)
    .bind(&template.description)
    .bind(&endpoint_url)
    .bind(&template.transport_type)
    .bind(&template.auth_type)
    .bind(&auth_encrypted)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await?;

    // d. Insert into mcp_store_installs
    sqlx::query(
        "INSERT INTO mcp_store_installs (template_id, server_id, installed_by) VALUES ($1, $2, $3)",
    )
    .bind(template.id)
    .bind(server.id)
    .bind(auth_user.claims.sub)
    .execute(&state.db)
    .await?;

    // e. Increment install_count on template
    sqlx::query("UPDATE mcp_store_templates SET install_count = install_count + 1 WHERE id = $1")
        .bind(template.id)
        .execute(&state.db)
        .await?;

    // Sync the in-memory MCP registry
    if let Ok(registered) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &server,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(registered).await;
        state.mcp_circuit_breakers.register(&server.name).await;
    }

    // f. Trigger tool discovery in background (same pattern as create_server)
    {
        let db = state.db.clone();
        let key = state.config.encryption_key.clone();
        let http = state.http_client.clone();
        let registry = state.mcp_registry.clone();
        let server = server.clone();
        let server_id = server.id;
        let db_for_err = state.db.clone();
        tokio::spawn(async move {
            match crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server, &key).await {
                Ok(n) => {
                    tracing::info!(
                        mcp_server = %server.name,
                        tools = n,
                        "MCP tool discovery completed for store-installed server"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                    if let Ok(updated) =
                        crate::mcp_runtime::build_registered_server(&db, &server, &key).await
                    {
                        registry.register(updated).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "MCP tool discovery failed for store-installed server"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
                        .bind(format!("{e}"))
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                }
            }
        });
    }

    state.audit.log(
        auth_user
            .audit("mcp_store.installed")
            .resource("mcp_server")
            .resource_id(server.id.to_string())
            .detail(serde_json::json!({
                "template_slug": &slug,
                "template_name": &template.name,
            })),
    );

    Ok(Json(server))
}

// ---------------------------------------------------------------------------
// Remote registry sync
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegistryResponse {
    #[allow(dead_code)]
    version: Option<i32>,
    templates: Vec<RegistryTemplate>,
}

/// Extract a localized string — accepts either `"plain"` or `{"en": "...", "zh": "..."}`.
/// Returns `"en | zh"` joined, or the plain string.
fn flatten_i18n(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) => {
            let en = map.get("en").and_then(|v| v.as_str()).unwrap_or("");
            let zh = map.get("zh").and_then(|v| v.as_str()).unwrap_or("");
            if en.is_empty() && zh.is_empty() {
                None
            } else {
                // Store both languages separated by \n---\n for the frontend to split
                Some(format!("{en}\n---\n{zh}"))
            }
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct RegistryTemplate {
    slug: String,
    name: String,
    description: Option<serde_json::Value>,
    category: Option<String>,
    tags: Option<Vec<String>>,
    endpoint_template: Option<String>,
    transport_type: Option<String>,
    auth_type: Option<String>,
    auth_instructions: Option<serde_json::Value>,
    deploy_type: Option<String>,
    deploy_command: Option<String>,
    deploy_docs_url: Option<String>,
    homepage_url: Option<String>,
    repo_url: Option<String>,
    featured: Option<bool>,
}

/// POST /api/admin/mcp-store/sync — sync templates from a remote registry.
/// If no body is provided, uses the configured `mcp_store.registry_url` setting.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SyncRegistryRequest {
    pub registry_url: Option<String>,
}

pub async fn sync_registry(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<SyncRegistryRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("settings:write")?;
    auth_user
        .assert_scope_global(&state.db, "settings:write")
        .await?;

    let url = match req.registry_url {
        Some(ref u) if !u.is_empty() => u.clone(),
        _ => state
            .dynamic_config
            .get_string("mcp_store.registry_url")
            .await
            .unwrap_or_default(),
    };

    if url.is_empty() {
        return Err(AppError::BadRequest(
            "No registry URL configured. Set mcp_store.registry_url in settings or provide registry_url in the request body.".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client error: {e}")))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to fetch registry: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "Registry returned HTTP {}",
            resp.status()
        )));
    }

    let registry: RegistryResponse = resp
        .json()
        .await
        .map_err(|e| AppError::BadRequest(format!("Invalid registry JSON: {e}")))?;

    let mut synced = 0u32;
    for t in &registry.templates {
        sqlx::query(
            r#"INSERT INTO mcp_store_templates
               (slug, name, description, category, tags, endpoint_template,
                transport_type, auth_type, auth_instructions, deploy_type,
                deploy_command, deploy_docs_url, homepage_url, repo_url, featured, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, now())
               ON CONFLICT (slug) DO UPDATE SET
                 name = EXCLUDED.name,
                 description = EXCLUDED.description,
                 category = EXCLUDED.category,
                 tags = EXCLUDED.tags,
                 endpoint_template = EXCLUDED.endpoint_template,
                 transport_type = EXCLUDED.transport_type,
                 auth_type = EXCLUDED.auth_type,
                 auth_instructions = EXCLUDED.auth_instructions,
                 deploy_type = EXCLUDED.deploy_type,
                 deploy_command = EXCLUDED.deploy_command,
                 deploy_docs_url = EXCLUDED.deploy_docs_url,
                 homepage_url = EXCLUDED.homepage_url,
                 repo_url = EXCLUDED.repo_url,
                 featured = EXCLUDED.featured,
                 updated_at = now()"#,
        )
        .bind(&t.slug)
        .bind(&t.name)
        .bind(t.description.as_ref().and_then(flatten_i18n).as_deref())
        .bind(&t.category)
        .bind(t.tags.as_deref().unwrap_or(&[]))
        .bind(&t.endpoint_template)
        .bind(t.transport_type.as_deref().unwrap_or("streamable_http"))
        .bind(&t.auth_type)
        .bind(
            t.auth_instructions
                .as_ref()
                .and_then(flatten_i18n)
                .as_deref(),
        )
        .bind(t.deploy_type.as_deref().unwrap_or("hosted"))
        .bind(&t.deploy_command)
        .bind(&t.deploy_docs_url)
        .bind(&t.homepage_url)
        .bind(&t.repo_url)
        .bind(t.featured.unwrap_or(false))
        .execute(&state.db)
        .await?;
        synced += 1;
    }

    state.audit.log(
        auth_user
            .audit("mcp_store.synced")
            .resource("mcp_store")
            .detail(serde_json::json!({ "registry_url": url, "synced": synced })),
    );

    Ok(Json(
        serde_json::json!({"status": "synced", "count": synced, "registry_url": url}),
    ))
}
