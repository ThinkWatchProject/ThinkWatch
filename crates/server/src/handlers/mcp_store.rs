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
