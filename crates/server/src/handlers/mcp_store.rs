use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::models::McpStoreTemplate;

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
// GET /api/mcp/store/{slug} — single template
// ---------------------------------------------------------------------------

/// Returns one template by slug so the registration wizard can prefill
/// its fields when arriving from the store via
/// `/mcp/servers/new?template={slug}`. The wizard uses the response to
/// pre-populate `endpoint_url`, `auth_shape`, OAuth endpoints / scopes
/// and the `auth_header_*` defaults; admin can override anything before
/// committing.
pub async fn get_template(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<McpStoreTemplate>, AppError> {
    let template =
        sqlx::query_as::<_, McpStoreTemplate>("SELECT * FROM mcp_store_templates WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Template '{slug}' not found")))?;
    Ok(Json(template))
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
    oauth_userinfo_endpoint: Option<String>,
    oauth_default_scopes: Option<Vec<String>>,
    /// Single-valued auth shape — `'anonymous'`, `'oauth'`, or
    /// `'static'`. When omitted, the registry parser derives it from
    /// the OAuth fields (`oauth_issuer set` ⇒ `'oauth'`, else
    /// `'anonymous'`).
    auth_shape: Option<String>,
    static_token_help_url: Option<String>,
    /// Optional header overrides — defaults to `Authorization` /
    /// `Bearer {{token}}` when omitted.
    auth_header_name: Option<String>,
    auth_value_template: Option<String>,
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
    auth_user
        .require_global_permission(&state.db, "settings:write")
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

    // SSRF defense: the URL comes from admin input (either the saved
    // setting or a per-request override). Block private CIDRs +
    // metadata endpoints + reject `http://` to avoid downgrade. The
    // `settings:write` permission is a broad-scope knob and not a
    // sufficient gate against an internal-fetch primitive.
    think_watch_common::validation::validate_url(&url)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // Don't follow redirects — a registry host returning
        // `302 Location: http://169.254.169.254/...` would silently
        // bypass the validate_url check above. Force callers to
        // surface redirects explicitly if they ever become needed.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client error: {e}")))?;

    // Hard wall-clock cap on top of the per-request timeout so a slow body
    // stream can't keep the Axum worker hung past 8s.
    let resp = match tokio::time::timeout(
        std::time::Duration::from_secs(8),
        client.get(&url).send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(AppError::BadRequest(format!(
                "Failed to fetch registry: {e}"
            )));
        }
        Err(_) => {
            return Err(AppError::BadRequest(
                "Registry sync timed out — the registry URL may be unreachable.".into(),
            ));
        }
    };

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

    // Wrap the whole sync (upserts + post-loop cleanup DELETE) in a
    // single transaction. Previously each `INSERT ... ON CONFLICT
    // DO UPDATE` ran on its own pooled connection, so a mid-batch
    // failure (PG hiccup, bad row #50 out of 100) left the catalog
    // in a half-synced state: rows 1..49 carried the new revision,
    // 50+ carried the old, and the trailing cleanup DELETE then
    // either ran against the half-state (orphaning slugs that were
    // about to be re-upserted) or failed too. One TX = atomic
    // catalog revision swap.
    let mut tx = state.db.begin().await?;
    let mut synced = 0u32;
    for t in &registry.templates {
        let auth_header_name = t
            .auth_header_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Authorization".to_string());
        let auth_value_template = t
            .auth_value_template
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Bearer {{token}}".to_string());
        // Reject malformed templates from upstream registries early —
        // a typo in a community template shouldn't break gateway boot.
        if let Err(e) =
            think_watch_mcp_gateway::user_token::validate_auth_value_template(&auth_value_template)
        {
            tracing::warn!(slug = %t.slug, error = %e, "skipping template with invalid auth_value_template");
            continue;
        }

        // Derive auth_shape from the registry payload — explicit
        // value wins; otherwise OAuth issuer presence implies 'oauth';
        // anonymous as the safest fallback. Templates with a static
        // help URL but no shape declaration are coerced to 'static'.
        let auth_shape = match t.auth_shape.as_deref() {
            Some("oauth") | Some("static") | Some("anonymous") => t.auth_shape.clone().unwrap(),
            _ if t.oauth_issuer.is_some() => "oauth".to_string(),
            _ if t.static_token_help_url.is_some() => "static".to_string(),
            _ => "anonymous".to_string(),
        };

        sqlx::query(
            r#"INSERT INTO mcp_store_templates
               (slug, name, description, category, tags, endpoint_template,
                oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
                oauth_revocation_endpoint, oauth_userinfo_endpoint,
                oauth_default_scopes,
                auth_shape, static_token_help_url,
                auth_header_name, auth_value_template,
                auth_instructions, deploy_type,
                deploy_command, deploy_docs_url, homepage_url, repo_url, featured, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                       $16, $17, $18, $19, $20, $21, $22, $23, now())
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
                 oauth_userinfo_endpoint = EXCLUDED.oauth_userinfo_endpoint,
                 oauth_default_scopes = EXCLUDED.oauth_default_scopes,
                 auth_shape = EXCLUDED.auth_shape,
                 static_token_help_url = EXCLUDED.static_token_help_url,
                 auth_header_name = EXCLUDED.auth_header_name,
                 auth_value_template = EXCLUDED.auth_value_template,
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
        .bind(&t.oauth_userinfo_endpoint)
        .bind(t.oauth_default_scopes.as_deref().unwrap_or(&[]))
        .bind(&auth_shape)
        .bind(&t.static_token_help_url)
        .bind(&auth_header_name)
        .bind(&auth_value_template)
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
        .execute(&mut *tx)
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
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

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
