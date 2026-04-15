//! Policy-as-Code — YAML import / export for `rbac_roles`.
//!
//! Serialisation shape deliberately mimics Kubernetes resources so the
//! files drop into GitOps / CI review workflows without surprises:
//!
//! ```yaml
//! apiVersion: thinkwatch.dev/v1
//! kind: Role
//! metadata:
//!   name: developer
//!   description: Standard developer role
//! spec:
//!   permissions: [api_keys:read, api_keys:create]
//!   allowed_models: [gpt-4o, claude-sonnet-4]
//!   allowed_mcp_tools: null
//!   policy_document: null
//! ```
//!
//! `metadata.name` is the stable identity key for upsert. Rotation of
//! id / created_at / created_by are deliberately *not* in the file —
//! they're operational state, not desired-state config.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::rbac;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

const API_VERSION: &str = "thinkwatch.dev/v1";
const KIND: &str = "Role";

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RolePolicyDocument {
    /// Must equal `thinkwatch.dev/v1`. Anything else is rejected so an
    /// accidental paste of a Kubernetes manifest can't collide with role
    /// import semantics.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Must equal `Role`.
    pub kind: String,
    pub metadata: RolePolicyMetadata,
    pub spec: RolePolicySpec,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RolePolicyMetadata {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema, Default)]
pub struct RolePolicySpec {
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_mcp_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_document: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// GET /api/admin/roles/{id}/export — serialise one role to YAML
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/roles/{id}/export",
    tag = "Roles",
    params(("id" = uuid::Uuid, Path, description = "Role ID")),
    responses(
        (status = 200, description = "YAML document representing the role"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn export_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    auth_user.require_permission("roles:read")?;

    type ExportRow = (
        String,
        Option<String>,
        Vec<String>,
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<serde_json::Value>,
    );
    let row: Option<ExportRow> = sqlx::query_as(
        "SELECT name, description, permissions, allowed_models, \
                allowed_mcp_tools, policy_document \
           FROM rbac_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;

    let (name, description, permissions, allowed_models, allowed_mcp_tools, policy_document) =
        row.ok_or_else(|| AppError::NotFound(format!("Role {id} not found")))?;

    let doc = RolePolicyDocument {
        api_version: API_VERSION.into(),
        kind: KIND.into(),
        metadata: RolePolicyMetadata {
            name: name.clone(),
            description,
        },
        spec: RolePolicySpec {
            permissions,
            allowed_models,
            allowed_mcp_tools,
            policy_document,
        },
    };

    let yaml = serde_yml::to_string(&doc)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("YAML serialise failed: {e}")))?;

    let filename = format!("role-{}.yaml", sanitise_filename(&name));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/yaml; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(yaml))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Response build: {e}")))?;
    Ok(response)
}

/// Strip anything outside [a-zA-Z0-9._-] so a role named "../etc/passwd"
/// can't steer the Content-Disposition filename on the client. Lossy on
/// purpose — the DB still has the real name for reference.
fn sanitise_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// POST /api/admin/roles/import — upsert from YAML
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ImportQuery {
    /// When true, validate + diff without modifying the DB.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ImportResult {
    pub role_name: String,
    /// One of "created" / "updated" / "unchanged" / "dry_run_*".
    pub outcome: String,
    /// Changed field names when updating — empty when creating or
    /// unchanged. Populated on dry_run against an existing role too.
    pub diff_fields: Vec<String>,
    /// ID of the affected row when not dry_run; null otherwise.
    pub role_id: Option<Uuid>,
}

/// Accepts a raw YAML body on POST. Content-Type is ignored — we parse
/// the bytes as YAML regardless — so `curl --data-binary @file.yaml`
/// works without gymnastics.
#[utoipa::path(
    post,
    path = "/api/admin/roles/import",
    tag = "Roles",
    params(ImportQuery),
    request_body(content = String, content_type = "application/yaml"),
    responses(
        (status = 200, description = "Import result (dry-run or applied)", body = ImportResult),
        (status = 400, description = "Invalid YAML or policy"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn import_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ImportQuery>,
    body: String,
) -> Result<Json<ImportResult>, AppError> {
    auth_user.require_permission("roles:create")?;
    auth_user.require_permission("roles:update")?;
    auth_user
        .assert_scope_global(&state.db, "roles:create")
        .await?;

    let doc: RolePolicyDocument = serde_yml::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("YAML parse failed: {e}")))?;

    if doc.api_version != API_VERSION {
        return Err(AppError::BadRequest(format!(
            "Unsupported apiVersion '{}' (expected '{API_VERSION}')",
            doc.api_version
        )));
    }
    if doc.kind != KIND {
        return Err(AppError::BadRequest(format!(
            "Unsupported kind '{}' (expected '{KIND}')",
            doc.kind
        )));
    }

    let name = doc.metadata.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "metadata.name must be 1-100 characters".into(),
        ));
    }
    for perm in &doc.spec.permissions {
        if !crate::handlers::roles::is_known_permission(perm) {
            return Err(AppError::BadRequest(format!("Invalid permission: {perm}")));
        }
    }
    if let Some(ref pd) = doc.spec.policy_document {
        rbac::validate_policy_document(pd).map_err(AppError::BadRequest)?;
    }

    // Look up existing by name (the stable identity key in the file).
    type ImportLookupRow = (
        Uuid,
        Option<String>,
        Vec<String>,
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<serde_json::Value>,
        bool,
    );
    let existing: Option<ImportLookupRow> = sqlx::query_as(
        "SELECT id, description, permissions, allowed_models, \
                allowed_mcp_tools, policy_document, is_system \
           FROM rbac_roles WHERE name = $1",
    )
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;

    if let Some((id, ex_desc, ex_perms, ex_models, ex_tools, ex_policy, is_system)) = existing {
        if is_system {
            return Err(AppError::BadRequest(
                "System roles cannot be modified via import".into(),
            ));
        }
        // Compute diff before deciding what to do.
        let mut diff: Vec<String> = Vec::new();
        if doc.metadata.description != ex_desc {
            diff.push("description".into());
        }
        if !same_string_set(&doc.spec.permissions, &ex_perms) {
            diff.push("permissions".into());
        }
        if !same_opt_string_set(&doc.spec.allowed_models, &ex_models) {
            diff.push("allowed_models".into());
        }
        if !same_opt_string_set(&doc.spec.allowed_mcp_tools, &ex_tools) {
            diff.push("allowed_mcp_tools".into());
        }
        if doc.spec.policy_document != ex_policy {
            diff.push("policy_document".into());
        }

        if q.dry_run {
            let outcome = if diff.is_empty() {
                "dry_run_unchanged"
            } else {
                "dry_run_update"
            };
            return Ok(Json(ImportResult {
                role_name: name,
                outcome: outcome.into(),
                diff_fields: diff,
                role_id: Some(id),
            }));
        }

        if diff.is_empty() {
            return Ok(Json(ImportResult {
                role_name: name,
                outcome: "unchanged".into(),
                diff_fields: vec![],
                role_id: Some(id),
            }));
        }

        sqlx::query(
            "UPDATE rbac_roles SET \
                description = $2, permissions = $3, allowed_models = $4, \
                allowed_mcp_tools = $5, policy_document = $6, \
                updated_at = now() \
              WHERE id = $1",
        )
        .bind(id)
        .bind(&doc.metadata.description)
        .bind(&doc.spec.permissions)
        .bind(&doc.spec.allowed_models)
        .bind(&doc.spec.allowed_mcp_tools)
        .bind(&doc.spec.policy_document)
        .execute(&state.db)
        .await?;

        state.audit.log(
            auth_user
                .audit("role.imported.updated")
                .resource("role")
                .resource_id(id.to_string())
                .detail(serde_json::json!({ "name": name, "diff": diff })),
        );

        Ok(Json(ImportResult {
            role_name: name,
            outcome: "updated".into(),
            diff_fields: diff,
            role_id: Some(id),
        }))
    } else {
        if q.dry_run {
            return Ok(Json(ImportResult {
                role_name: name,
                outcome: "dry_run_create".into(),
                diff_fields: vec![],
                role_id: None,
            }));
        }

        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO rbac_roles (name, description, is_system, permissions, \
                                     allowed_models, allowed_mcp_tools, \
                                     policy_document, created_by) \
             VALUES ($1, $2, FALSE, $3, $4, $5, $6, $7) \
             RETURNING id",
        )
        .bind(&name)
        .bind(&doc.metadata.description)
        .bind(&doc.spec.permissions)
        .bind(&doc.spec.allowed_models)
        .bind(&doc.spec.allowed_mcp_tools)
        .bind(&doc.spec.policy_document)
        .bind(auth_user.claims.sub)
        .fetch_one(&state.db)
        .await?;

        state.audit.log(
            auth_user
                .audit("role.imported.created")
                .resource("role")
                .resource_id(id.to_string())
                .detail(serde_json::json!({ "name": name })),
        );

        Ok(Json(ImportResult {
            role_name: name,
            outcome: "created".into(),
            diff_fields: vec![],
            role_id: Some(id),
        }))
    }
}

fn same_string_set(a: &[String], b: &[String]) -> bool {
    use std::collections::HashSet;
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    sa == sb
}

fn same_opt_string_set(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => same_string_set(x, y),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> RolePolicyDocument {
        RolePolicyDocument {
            api_version: API_VERSION.into(),
            kind: KIND.into(),
            metadata: RolePolicyMetadata {
                name: "developer".into(),
                description: Some("Standard developer".into()),
            },
            spec: RolePolicySpec {
                permissions: vec!["api_keys:read".into(), "api_keys:create".into()],
                allowed_models: Some(vec!["gpt-4o".into()]),
                allowed_mcp_tools: None,
                policy_document: None,
            },
        }
    }

    #[test]
    fn yaml_round_trip_preserves_document() {
        let yaml = serde_yml::to_string(&sample_doc()).unwrap();
        let parsed: RolePolicyDocument = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed.api_version, API_VERSION);
        assert_eq!(parsed.kind, KIND);
        assert_eq!(parsed.metadata.name, "developer");
        assert_eq!(parsed.spec.permissions.len(), 2);
        assert_eq!(
            parsed.spec.allowed_models.as_deref(),
            Some(&["gpt-4o".to_string()][..])
        );
    }

    #[test]
    fn sanitise_filename_strips_path_traversal() {
        assert_eq!(sanitise_filename("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitise_filename("dev ops"), "dev_ops");
        assert_eq!(sanitise_filename("plain-name.v1"), "plain-name.v1");
    }

    #[test]
    fn same_string_set_is_order_insensitive() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        assert!(same_string_set(&a, &b));
        let c = vec!["a".to_string(), "b".to_string()];
        assert!(!same_string_set(&a, &c));
    }

    #[test]
    fn same_opt_string_set_treats_none_and_some_as_different() {
        let empty = Some(vec![]);
        assert!(!same_opt_string_set(&None, &empty));
        assert!(same_opt_string_set(&None, &None));
    }
}
