use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

// ============================================================================
// Multi-role merging semantics: UNION.
//
// A user with multiple role assignments gets the UNION of every role's
// `permissions` field. If ANY role grants a permission, the user has it.
//
// The same rule holds for `allowed_models` and `allowed_mcp_servers`:
// - If ANY role has `allowed_* = NULL` (unrestricted), the user is
//   unrestricted for that resource type.
// - Otherwise the effective allow-list is the union of every role's
//   allow-list.
//
// Rationale: RBAC is additive. Assigning a role should never *reduce*
// a user's access. "Least privilege" is expressed by not assigning the
// role in the first place, not by intersecting.
//
// `compute_user_permissions` below is the single source of truth for
// this rule. It is called at JWT creation time; the resulting union
// is embedded in `claims.permissions` and used by every runtime
// authorization check.
// ============================================================================

/// Load the union of permissions for every role assigned to `user_id`.
///
/// Returns a deduplicated, sorted list. Empty Vec if the user has no
/// roles (which is valid — they'll have no granular permissions and
/// every handler's `require_permission` call will reject them).
pub async fn compute_user_permissions(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    // Flatten the TEXT[] `permissions` field of every role the user
    // holds into a single set. `UNNEST` + `DISTINCT` does the job in
    // one round trip.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT perm \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
           CROSS JOIN LATERAL UNNEST(r.permissions) AS perm \
          WHERE ra.user_id = $1 \
          ORDER BY perm",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// Load the list of role NAMES (system + custom) assigned to `user_id`.
/// Used by the UI for badges and by `claims.roles`.
pub async fn load_user_role_names(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT r.name \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = $1 \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// Effective resource constraints for a user, derived by union'ing
/// every role's `allowed_models` and `allowed_mcp_servers`. Mirrors
/// the same union semantics documented at the top of this file:
///
///   - If ANY role has `allowed_* = NULL` (unrestricted), the field
///     in the result is also `None` and the gateway should treat the
///     user as unrestricted for that resource.
///   - Otherwise the result is the union of every restricted role's
///     allow-list, deduplicated.
///
/// This is what the gateway middleware merges with the per-API-key
/// allow-list (if any) before calling into the proxy.
pub struct UserResourceLimits {
    pub allowed_models: Option<Vec<String>>,
    pub allowed_mcp_servers: Option<Vec<Uuid>>,
}

pub async fn compute_user_resource_limits(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<UserResourceLimits, sqlx::Error> {
    type Row = (Option<Vec<String>>, Option<Vec<Uuid>>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.allowed_models, r.allowed_mcp_servers \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        // No roles → no constraints from the role side. The caller
        // will fall back to whatever the API key carries.
        return Ok(UserResourceLimits {
            allowed_models: None,
            allowed_mcp_servers: None,
        });
    }

    // Union with the "ANY null wins" rule: a single unrestricted
    // role makes the whole user unrestricted, since RBAC is additive.
    let mut models_unrestricted = false;
    let mut servers_unrestricted = false;
    let mut models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut servers: std::collections::BTreeSet<Uuid> = std::collections::BTreeSet::new();
    for (m, s) in rows {
        match m {
            None => models_unrestricted = true,
            Some(list) => models.extend(list),
        }
        match s {
            None => servers_unrestricted = true,
            Some(list) => servers.extend(list),
        }
    }
    Ok(UserResourceLimits {
        allowed_models: if models_unrestricted {
            None
        } else {
            Some(models.into_iter().collect())
        },
        allowed_mcp_servers: if servers_unrestricted {
            None
        } else {
            Some(servers.into_iter().collect())
        },
    })
}

// ---------------------------------------------------------------------------
// SystemRole — closed enum kept around for the setup wizard's hardcoded
// "assign super_admin to the first user" path and for tests that want a
// stable enum. NOT used for authorization anymore — the authoritative
// check reads `claims.permissions` via `AuthUser::require_permission`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemRole {
    SuperAdmin,
    Admin,
    TeamManager,
    Developer,
    Viewer,
}

impl SystemRole {
    pub fn as_str(&self) -> &str {
        match self {
            SystemRole::SuperAdmin => "super_admin",
            SystemRole::Admin => "admin",
            SystemRole::TeamManager => "team_manager",
            SystemRole::Developer => "developer",
            SystemRole::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "super_admin" => Some(SystemRole::SuperAdmin),
            "admin" => Some(SystemRole::Admin),
            "team_manager" => Some(SystemRole::TeamManager),
            "developer" => Some(SystemRole::Developer),
            "viewer" => Some(SystemRole::Viewer),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AWS IAM-style policy engine
// ---------------------------------------------------------------------------

/// AWS IAM-style policy document.
///
/// ```json
/// {
///   "Version": "2024-01-01",
///   "Statement": [
///     { "Sid": "AllowGateway", "Effect": "Allow", "Action": ["ai_gateway:*"], "Resource": ["*"] },
///     { "Sid": "DenyProviderWrite", "Effect": "Deny", "Action": ["providers:write"], "Resource": ["*"] }
///   ]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyDocument {
    pub version: String,
    pub statement: Vec<Statement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Statement {
    #[serde(default)]
    pub sid: Option<String>,
    pub effect: Effect,
    pub action: ActionPattern,
    pub resource: ResourcePattern,
    #[serde(default)]
    pub condition: Option<serde_json::Value>,
}

/// Allow or Deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
}

/// One or more action patterns. Supports `"*"` and glob like `"providers:*"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionPattern {
    Single(String),
    Multiple(Vec<String>),
}

impl ActionPattern {
    pub fn patterns(&self) -> &[String] {
        match self {
            ActionPattern::Single(s) => std::slice::from_ref(s),
            ActionPattern::Multiple(v) => v,
        }
    }
}

/// One or more resource patterns. Supports `"*"`, `"model:gpt-4o"`, `"mcp_server:<uuid>"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResourcePattern {
    Single(String),
    Multiple(Vec<String>),
}

impl ResourcePattern {
    pub fn patterns(&self) -> &[String] {
        match self {
            ResourcePattern::Single(s) => std::slice::from_ref(s),
            ResourcePattern::Multiple(v) => v,
        }
    }
}

/// Result of evaluating a single statement against a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyResult {
    Allow,
    Deny,
    NoMatch,
}

/// Glob-style pattern matching for action/resource strings.
/// Supports `*` as a wildcard that matches any sequence of characters.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Simple glob: split on '*' and match segments in order
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        // No wildcard — exact match
        return pattern == value;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = value[pos..].find(part) {
            if i == 0 && found != 0 {
                // First segment must be a prefix
                return false;
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }
    // If the last segment is non-empty, value must end after it
    if let Some(last) = parts.last()
        && !last.is_empty()
    {
        return pos == value.len();
    }
    true
}

/// Evaluate a single statement against an action and resource.
fn evaluate_statement(stmt: &Statement, action: &str, resource: &str) -> PolicyResult {
    let action_matches = stmt.action.patterns().iter().any(|p| glob_match(p, action));
    if !action_matches {
        return PolicyResult::NoMatch;
    }
    let resource_matches = stmt
        .resource
        .patterns()
        .iter()
        .any(|p| glob_match(p, resource));
    if !resource_matches {
        return PolicyResult::NoMatch;
    }
    match stmt.effect {
        Effect::Allow => PolicyResult::Allow,
        Effect::Deny => PolicyResult::Deny,
    }
}

/// Evaluate a complete policy document.
/// Returns the final decision for the given action/resource.
///
/// AWS rules: explicit Deny always wins. If no statement matches → implicit deny.
pub fn evaluate_policy(policy: &PolicyDocument, action: &str, resource: &str) -> PolicyResult {
    let mut has_allow = false;
    for stmt in &policy.statement {
        match evaluate_statement(stmt, action, resource) {
            PolicyResult::Deny => return PolicyResult::Deny,
            PolicyResult::Allow => has_allow = true,
            PolicyResult::NoMatch => {}
        }
    }
    if has_allow {
        PolicyResult::Allow
    } else {
        PolicyResult::NoMatch
    }
}

/// Evaluate multiple policy documents (e.g. from multiple attached roles).
/// Deny in ANY policy → denied. Allow in any + no deny → allowed. Otherwise → denied.
pub fn evaluate_policies(policies: &[PolicyDocument], action: &str, resource: &str) -> bool {
    let mut has_allow = false;
    for policy in policies {
        match evaluate_policy(policy, action, resource) {
            PolicyResult::Deny => return false,
            PolicyResult::Allow => has_allow = true,
            PolicyResult::NoMatch => {}
        }
    }
    has_allow
}

/// Validate a policy document JSON value. Returns a user-friendly error message on failure.
pub fn validate_policy_document(value: &serde_json::Value) -> Result<PolicyDocument, String> {
    // Size guard: reject excessively large policy documents (max 64 KB serialized)
    let raw = serde_json::to_string(value).unwrap_or_default();
    if raw.len() > 65_536 {
        return Err("Policy document too large (max 64 KB)".into());
    }

    let doc: PolicyDocument =
        serde_json::from_value(value.clone()).map_err(|e| format!("Invalid policy JSON: {e}"))?;

    if doc.statement.is_empty() {
        return Err("Policy must contain at least one Statement".into());
    }
    if doc.statement.len() > 100 {
        return Err("Policy contains too many statements (max 100)".into());
    }

    for (i, stmt) in doc.statement.iter().enumerate() {
        if stmt.action.patterns().is_empty() {
            return Err(format!("Statement[{i}]: Action must not be empty"));
        }
        if stmt.resource.patterns().is_empty() {
            return Err(format!("Statement[{i}]: Resource must not be empty"));
        }
        if stmt.action.patterns().len() > 50 {
            return Err(format!("Statement[{i}]: Too many action patterns (max 50)"));
        }
        if stmt.resource.patterns().len() > 50 {
            return Err(format!(
                "Statement[{i}]: Too many resource patterns (max 50)"
            ));
        }
        for action in stmt.action.patterns() {
            if action.is_empty() {
                return Err(format!("Statement[{i}]: Action pattern must not be empty"));
            }
        }
        for resource in stmt.resource.patterns() {
            if resource.is_empty() {
                return Err(format!(
                    "Statement[{i}]: Resource pattern must not be empty"
                ));
            }
        }
    }

    Ok(doc)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- SystemRole tests ---

    #[test]
    fn role_string_roundtrip() {
        let roles = [
            SystemRole::SuperAdmin,
            SystemRole::Admin,
            SystemRole::TeamManager,
            SystemRole::Developer,
            SystemRole::Viewer,
        ];
        for role in &roles {
            let s = role.as_str();
            let parsed = SystemRole::parse(s);
            assert_eq!(parsed.as_ref(), Some(role), "roundtrip failed for {s}");
        }
    }

    // --- IAM policy engine tests ---

    #[test]
    fn glob_match_wildcard() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("providers:*", "providers:read"));
        assert!(glob_match("providers:*", "providers:write"));
        assert!(!glob_match("providers:*", "mcp_servers:read"));
        assert!(glob_match("*:read", "providers:read"));
        assert!(glob_match("*:read", "analytics:read"));
        assert!(!glob_match("*:read", "providers:write"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("providers:read", "providers:read"));
        assert!(!glob_match("providers:read", "providers:write"));
    }

    #[test]
    fn policy_allow_basic() {
        let doc = PolicyDocument {
            version: "2024-01-01".into(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: ActionPattern::Multiple(vec!["ai_gateway:use".into()]),
                resource: ResourcePattern::Single("*".into()),
                condition: None,
            }],
        };
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "*"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "providers:write", "*"),
            PolicyResult::NoMatch
        );
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let doc = PolicyDocument {
            version: "2024-01-01".into(),
            statement: vec![
                Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: ActionPattern::Single("providers:*".into()),
                    resource: ResourcePattern::Single("*".into()),
                    condition: None,
                },
                Statement {
                    sid: None,
                    effect: Effect::Deny,
                    action: ActionPattern::Single("providers:write".into()),
                    resource: ResourcePattern::Single("*".into()),
                    condition: None,
                },
            ],
        };
        assert_eq!(
            evaluate_policy(&doc, "providers:read", "*"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "providers:write", "*"),
            PolicyResult::Deny
        );
    }

    #[test]
    fn policy_resource_scoping() {
        let doc = PolicyDocument {
            version: "2024-01-01".into(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: ActionPattern::Single("ai_gateway:use".into()),
                resource: ResourcePattern::Multiple(vec![
                    "model:gpt-4o".into(),
                    "model:claude-*".into(),
                ]),
                condition: None,
            }],
        };
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "model:gpt-4o"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "model:claude-sonnet"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "model:gemini-pro"),
            PolicyResult::NoMatch
        );
    }

    #[test]
    fn multiple_policies_deny_wins() {
        let allow = PolicyDocument {
            version: "2024-01-01".into(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: ActionPattern::Single("*".into()),
                resource: ResourcePattern::Single("*".into()),
                condition: None,
            }],
        };
        let deny = PolicyDocument {
            version: "2024-01-01".into(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Deny,
                action: ActionPattern::Single("system:*".into()),
                resource: ResourcePattern::Single("*".into()),
                condition: None,
            }],
        };
        assert!(evaluate_policies(
            &[allow.clone(), deny.clone()],
            "providers:read",
            "*"
        ));
        assert!(!evaluate_policies(&[allow, deny], "system:settings", "*"));
    }

    #[test]
    fn validate_policy_errors() {
        let empty = serde_json::json!({ "Version": "2024-01-01", "Statement": [] });
        assert!(validate_policy_document(&empty).is_err());

        let bad_action = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{ "Effect": "Allow", "Action": [], "Resource": ["*"] }]
        });
        assert!(validate_policy_document(&bad_action).is_err());
    }

    #[test]
    fn policy_json_roundtrip() {
        let json = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [
                {
                    "Sid": "AllowGateway",
                    "Effect": "Allow",
                    "Action": ["ai_gateway:use", "mcp_gateway:use"],
                    "Resource": ["*"]
                },
                {
                    "Effect": "Deny",
                    "Action": "system:*",
                    "Resource": "*"
                }
            ]
        });
        let doc = validate_policy_document(&json).expect("should parse");
        assert_eq!(doc.statement.len(), 2);
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "*"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "system:settings", "*"),
            PolicyResult::Deny
        );
    }
}
