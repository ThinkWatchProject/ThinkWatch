use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    /// Optional overrides for name + namespace_prefix. Frontend pre-resolves
    /// collisions and passes the already-deconflicted values; backend still
    /// validates uniqueness so concurrent installs stay safe.
    pub name: Option<String>,
    pub namespace_prefix: Option<String>,
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

    think_watch_common::validation::validate_url(&endpoint_url)?;
    if let Some(ref headers) = req.custom_headers {
        think_watch_common::validation::validate_custom_headers(headers)?;
    }

    // Verify the endpoint actually responds to JSON-RPC tools/list before
    // we persist anything. Anonymous probe — for OAuth / static-token
    // upstreams the call may fail with 401, in which case the operator
    // can still proceed (the per-user auth happens after install when
    // a user authorizes via /connections).
    let http = state.http_client.load();
    let probe =
        super::mcp_shared::probe_mcp_endpoint(&http, &endpoint_url, req.custom_headers.as_ref())
            .await;
    let probe_failed_with_auth =
        probe.message.starts_with("HTTP 401") || probe.message.starts_with("HTTP 403");
    if !probe.success && !probe_failed_with_auth {
        return Err(AppError::BadRequest(format!(
            "Connection test failed: {}",
            probe.message
        )));
    }

    // Auto-detect transport type for hosted endpoints. Anonymous probe.
    let transport_type = {
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(&http_detect, &endpoint_url, None)
            .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => "streamable_http".to_owned(),
        }
    };

    // Build config_json
    let config_json = {
        let mut config = serde_json::json!({});
        if let Some(ref headers) = req.custom_headers {
            think_watch_common::validation::validate_custom_headers(headers)?;
            config["custom_headers"] = serde_json::to_value(headers).unwrap_or_default();
        }
        config
    };

    // c–e. Server creation, install record, and counter bump go
    // through the shared `install_template_into_db` helper so the
    // exact same TX shape is exercised by the integration suite.
    let server = install_template_into_db(
        &state.db,
        template.id,
        &slug,
        &endpoint_url,
        &transport_type,
        config_json,
        req.name.as_deref(),
        req.namespace_prefix.as_deref(),
        auth_user.claims.sub,
    )
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
        let http = (**state.http_client.load()).clone();
        let registry = state.mcp_registry.clone();
        let server = server.clone();
        let server_id = server.id;
        let db_for_err = state.db.clone();
        tokio::spawn(async move {
            match crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server).await {
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
    // Present in the registry JSON; serde requires the field for deserialization.
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
    oauth_issuer: Option<String>,
    oauth_authorization_endpoint: Option<String>,
    oauth_token_endpoint: Option<String>,
    oauth_revocation_endpoint: Option<String>,
    oauth_default_scopes: Option<Vec<String>>,
    allow_static_token: Option<bool>,
    static_token_help_url: Option<String>,
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
                oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
                oauth_revocation_endpoint, oauth_default_scopes,
                allow_static_token, static_token_help_url,
                auth_instructions, deploy_type,
                deploy_command, deploy_docs_url, homepage_url, repo_url, featured, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                       $16, $17, $18, $19, $20, now())
               ON CONFLICT (slug) DO UPDATE SET
                 name = EXCLUDED.name,
                 description = EXCLUDED.description,
                 category = EXCLUDED.category,
                 tags = EXCLUDED.tags,
                 endpoint_template = EXCLUDED.endpoint_template,
                 oauth_issuer = EXCLUDED.oauth_issuer,
                 oauth_authorization_endpoint = EXCLUDED.oauth_authorization_endpoint,
                 oauth_token_endpoint = EXCLUDED.oauth_token_endpoint,
                 oauth_revocation_endpoint = EXCLUDED.oauth_revocation_endpoint,
                 oauth_default_scopes = EXCLUDED.oauth_default_scopes,
                 allow_static_token = EXCLUDED.allow_static_token,
                 static_token_help_url = EXCLUDED.static_token_help_url,
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
        .bind(&t.oauth_issuer)
        .bind(&t.oauth_authorization_endpoint)
        .bind(&t.oauth_token_endpoint)
        .bind(&t.oauth_revocation_endpoint)
        .bind(t.oauth_default_scopes.as_deref().unwrap_or(&[]))
        .bind(t.allow_static_token.unwrap_or(false))
        .bind(&t.static_token_help_url)
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

    // Remove templates that are no longer in the registry (but keep those with active installs)
    let registry_slugs: Vec<&str> = registry.templates.iter().map(|t| t.slug.as_str()).collect();
    let removed = sqlx::query_scalar::<_, i64>(
        r#"WITH deleted AS (
             DELETE FROM mcp_store_templates
             WHERE slug != ALL($1)
               AND id NOT IN (SELECT template_id FROM mcp_store_installs)
             RETURNING 1
           )
           SELECT COUNT(*) FROM deleted"#,
    )
    .bind(&registry_slugs)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("mcp_store.synced")
            .resource("mcp_store")
            .detail(
                serde_json::json!({ "registry_url": url, "synced": synced, "removed": removed }),
            ),
    );

    Ok(Json(
        serde_json::json!({"status": "synced", "count": synced, "removed": removed, "registry_url": url}),
    ))
}

/// Process-wide advisory-lock key for the install path. Any 64-bit
/// constant works; the literal spells "mcpStore" in ASCII so a DBA
/// glancing at `pg_locks` can tell what's holding the row.
const INSTALL_LOCK_KEY: i64 = 0x6D637053746F7265;

/// Persist a store-template install in one transaction:
/// 1. acquire a process-wide advisory lock so concurrent installs
///    of templates that happen to share a default name can never
///    both grab the same `(name, namespace_prefix)` slot;
/// 2. re-fetch the template `FOR UPDATE` so a concurrent
///    `sync_registry` deletion can't pull the row out from under us;
/// 3. resolve a free `(name, prefix)` pair, INSERT the server,
///    INSERT the install record, bump `install_count`.
///
/// Exposed so integration tests can drive the TX without going
/// through the HTTP probe + SSRF guard the public handler runs first.
#[allow(clippy::too_many_arguments)]
pub async fn install_template_into_db(
    db: &sqlx::PgPool,
    template_id: uuid::Uuid,
    slug_for_error: &str,
    endpoint_url: &str,
    transport_type: &str,
    config_json: serde_json::Value,
    name_override: Option<&str>,
    prefix_override: Option<&str>,
    installed_by: Uuid,
) -> Result<McpServer, AppError> {
    let mut tx = db.begin().await?;

    // Serialize all install transactions globally on one advisory
    // lock. This is the "no count drift / no UNIQUE-violation"
    // guarantee — the FOR UPDATE on the template row alone only
    // serializes installs of the *same* template, leaving a race
    // when two different templates' default names collide.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(INSTALL_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    let template = sqlx::query_as::<_, McpStoreTemplate>(
        "SELECT * FROM mcp_store_templates WHERE id = $1 FOR UPDATE",
    )
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        AppError::NotFound(format!(
            "Template '{slug_for_error}' was removed during install"
        ))
    })?;

    let base_name = name_override
        .filter(|s| !s.is_empty())
        .unwrap_or(&template.name);
    let default_prefix = template.slug.replace('-', "_");
    let base_prefix = prefix_override
        .filter(|s| !s.is_empty())
        .unwrap_or(&default_prefix);
    super::mcp_shared::normalize_namespace_prefix(Some(base_prefix), base_name)?;
    let (resolved_name, resolved_prefix) =
        resolve_server_collisions(&mut tx, base_name, base_prefix).await?;

    // Copy the template's per-user auth shape onto the new server row.
    // OAuth client credentials still need to be filled in by the admin
    // afterwards (templates can ship endpoint URLs but not secrets).
    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (
               name, namespace_prefix, description, endpoint_url, transport_type,
               oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
               oauth_revocation_endpoint, oauth_scopes,
               allow_static_token, static_token_help_url,
               config_json
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
           RETURNING *"#,
    )
    .bind(&resolved_name)
    .bind(&resolved_prefix)
    .bind(&template.description)
    .bind(endpoint_url)
    .bind(transport_type)
    .bind(&template.oauth_issuer)
    .bind(&template.oauth_authorization_endpoint)
    .bind(&template.oauth_token_endpoint)
    .bind(&template.oauth_revocation_endpoint)
    .bind(&template.oauth_default_scopes)
    .bind(template.allow_static_token)
    .bind(&template.static_token_help_url)
    .bind(&config_json)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO mcp_store_installs (template_id, server_id, installed_by) VALUES ($1, $2, $3)",
    )
    .bind(template.id)
    .bind(server.id)
    .bind(installed_by)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE mcp_store_templates SET install_count = install_count + 1 WHERE id = $1")
        .bind(template.id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(server)
}

/// Find an available `(name, namespace_prefix)` pair by appending `_2`, `_3`, …
/// when the base values are already taken. Runs inside the caller's tx so
/// two concurrent installs of the same template can't pick the same suffix.
async fn resolve_server_collisions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    base_name: &str,
    base_prefix: &str,
) -> Result<(String, String), AppError> {
    for i in 1..100 {
        let (n, p) = if i == 1 {
            (base_name.to_owned(), base_prefix.to_owned())
        } else {
            (format!("{base_name} #{i}"), format!("{base_prefix}_{i}"))
        };
        // `SELECT 1` is INT4 on the wire; binding into `Option<i64>`
        // panics with a column-decode mismatch the moment a row
        // comes back. We don't actually care about the value — only
        // whether the row exists — so use Option<i32>.
        let conflict: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM mcp_servers WHERE name = $1 OR namespace_prefix = $2 LIMIT 1",
        )
        .bind(&n)
        .bind(&p)
        .fetch_optional(&mut **tx)
        .await?;
        if conflict.is_none() {
            return Ok((n, p));
        }
    }
    Err(AppError::BadRequest(
        "Too many installations of this template (>99) — remove some before installing again"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_i18n_passes_through_plain_strings() {
        assert_eq!(flatten_i18n(&json!("hello")).as_deref(), Some("hello"));
    }

    #[test]
    fn flatten_i18n_joins_bilingual_objects() {
        let v = json!({"en": "GitHub", "zh": "代码托管"});
        assert_eq!(flatten_i18n(&v).as_deref(), Some("GitHub\n---\n代码托管"));
    }

    #[test]
    fn flatten_i18n_handles_missing_language() {
        // Only en provided — zh side empty
        let v = json!({"en": "only english"});
        assert_eq!(flatten_i18n(&v).as_deref(), Some("only english\n---\n"));
    }

    #[test]
    fn flatten_i18n_returns_none_for_empty_object() {
        let v = json!({"en": "", "zh": ""});
        assert_eq!(flatten_i18n(&v), None);
    }

    #[test]
    fn flatten_i18n_returns_none_for_non_string_non_object() {
        assert_eq!(flatten_i18n(&json!(42)), None);
        assert_eq!(flatten_i18n(&json!(null)), None);
        assert_eq!(flatten_i18n(&json!([1, 2, 3])), None);
    }
}
