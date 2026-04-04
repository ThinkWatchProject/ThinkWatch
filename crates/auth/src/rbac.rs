use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Legacy SystemRole (kept for JWT-based role checks)
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

    pub fn has_permission(&self, resource: &str, action: &str) -> bool {
        match self {
            SystemRole::SuperAdmin => true,
            SystemRole::Admin => !matches!((resource, action), ("system", "configure_oidc")),
            SystemRole::TeamManager => matches!(
                (resource, action),
                ("ai_gateway", "use")
                    | ("mcp_gateway", "use")
                    | ("api_keys", "read")
                    | ("api_keys", "write")
                    | ("team", "read")
                    | ("team", "write")
                    | ("analytics", "read")
            ),
            SystemRole::Developer => matches!(
                (resource, action),
                ("ai_gateway", "use")
                    | ("mcp_gateway", "use")
                    | ("api_keys", "read")
                    | ("api_keys", "write")
                    | ("analytics", "read")
            ),
            SystemRole::Viewer => matches!(
                (resource, action),
                ("analytics", "read") | ("api_keys", "read")
            ),
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

/// Convert legacy flat permissions list to a policy document.
/// Useful for migration and backward compatibility.
pub fn permissions_to_policy(permissions: &[String]) -> PolicyDocument {
    PolicyDocument {
        version: "2024-01-01".into(),
        statement: vec![Statement {
            sid: Some("LegacyPermissions".into()),
            effect: Effect::Allow,
            action: ActionPattern::Multiple(permissions.to_vec()),
            resource: ResourcePattern::Single("*".into()),
            condition: None,
        }],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- SystemRole tests (unchanged) ---

    #[test]
    fn super_admin_has_all_permissions() {
        let role = SystemRole::SuperAdmin;
        assert!(role.has_permission("ai_gateway", "use"));
        assert!(role.has_permission("mcp_gateway", "use"));
        assert!(role.has_permission("system", "configure_oidc"));
        assert!(role.has_permission("analytics", "read"));
        assert!(role.has_permission("team", "write"));
        assert!(role.has_permission("anything", "whatever"));
    }

    #[test]
    fn viewer_can_only_read_analytics_and_api_keys() {
        let role = SystemRole::Viewer;
        assert!(role.has_permission("analytics", "read"));
        assert!(role.has_permission("api_keys", "read"));
        // Should not have write or other access
        assert!(!role.has_permission("ai_gateway", "use"));
        assert!(!role.has_permission("mcp_gateway", "use"));
        assert!(!role.has_permission("api_keys", "write"));
        assert!(!role.has_permission("team", "read"));
        assert!(!role.has_permission("system", "configure_oidc"));
    }

    #[test]
    fn developer_permissions() {
        let role = SystemRole::Developer;
        assert!(role.has_permission("ai_gateway", "use"));
        assert!(role.has_permission("mcp_gateway", "use"));
        assert!(role.has_permission("api_keys", "read"));
        assert!(role.has_permission("api_keys", "write"));
        assert!(role.has_permission("analytics", "read"));
        // Developer should NOT manage teams or configure system
        assert!(!role.has_permission("team", "read"));
        assert!(!role.has_permission("team", "write"));
        assert!(!role.has_permission("system", "configure_oidc"));
    }

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
    fn permissions_to_policy_roundtrip() {
        let perms = vec!["ai_gateway:use".into(), "providers:read".into()];
        let doc = permissions_to_policy(&perms);
        assert_eq!(
            evaluate_policy(&doc, "ai_gateway:use", "*"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "providers:read", "*"),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "system:settings", "*"),
            PolicyResult::NoMatch
        );
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
