use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use uuid::Uuid;

/// Key identifying a specific tool on a specific server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolKey {
    server_id: Uuid,
    tool_name: String,
}

/// A policy entry: if `allowed_roles` is non-empty only users with one of
/// those roles may invoke the tool.  An empty vec means "deny all except
/// super_admin".
#[derive(Debug, Clone)]
struct ToolPolicy {
    allowed_roles: Vec<String>,
}

/// In-memory, tool-level access controller.
///
/// Default behaviour is **deny all** for non-admin users. The roles
/// `"super_admin"` and `"admin"` always have access regardless of policy
/// (admins are responsible for setting up tool ACLs in the first place).
/// For any other role, the call is allowed only if an explicit policy
/// exists for the `(server, tool)` pair AND the user's roles intersect
/// the policy's allowed set.
///
/// This was changed from the previous default-allow behaviour because the
/// previous design exposed every newly-discovered MCP tool to every
/// authenticated user until an admin explicitly locked it down — a
/// dangerous default for a multi-tenant gateway.
#[derive(Clone)]
pub struct AccessController {
    /// Map from (server, tool) → policy. Absent entries mean "deny non-admins".
    policies: Arc<RwLock<HashMap<ToolKey, ToolPolicy>>>,
}

impl AccessController {
    pub fn new() -> Self {
        Self {
            policies: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check whether a user (identified by `user_id` and their `roles`) is
    /// allowed to call a specific tool on a specific server.
    ///
    /// Default-deny: a missing policy denies non-admin users.
    pub async fn check_tool_access(
        &self,
        _user_id: Uuid,
        server_id: Uuid,
        tool_name: &str,
        user_roles: &[String],
    ) -> bool {
        // Admins always pass — they're the ones managing ACLs.
        if user_roles
            .iter()
            .any(|r| r == "super_admin" || r == "admin")
        {
            return true;
        }

        let policies = self.policies.read().await;
        let key = ToolKey {
            server_id,
            tool_name: tool_name.to_owned(),
        };

        match policies.get(&key) {
            // Default-deny: no policy means non-admins can't call the tool.
            None => false,
            Some(policy) if policy.allowed_roles.is_empty() => false,
            Some(policy) => policy
                .allowed_roles
                .iter()
                .any(|allowed| user_roles.contains(allowed)),
        }
    }

    /// Convenience wrapper when the caller only has a `user_id` and no
    /// roles available. With default-deny semantics this will reject any
    /// non-admin user, since the empty role set can never satisfy a
    /// policy. Callers that have role information should use
    /// `check_tool_access` directly.
    pub async fn check_tool_access_by_id(
        &self,
        user_id: Uuid,
        server_id: Uuid,
        tool_name: &str,
    ) -> bool {
        self.check_tool_access(user_id, server_id, tool_name, &[])
            .await
    }

    /// Register (or overwrite) a policy for a tool.
    pub async fn set_policy(&self, server_id: Uuid, tool_name: String, allowed_roles: Vec<String>) {
        let mut policies = self.policies.write().await;
        policies.insert(
            ToolKey {
                server_id,
                tool_name,
            },
            ToolPolicy { allowed_roles },
        );
    }

    /// Remove a policy, returning the tool to default-allow behaviour.
    pub async fn remove_policy(&self, server_id: Uuid, tool_name: &str) {
        let mut policies = self.policies.write().await;
        policies.remove(&ToolKey {
            server_id,
            tool_name: tool_name.to_owned(),
        });
    }
}

impl Default for AccessController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_denies_non_admin() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        let allowed = ac
            .check_tool_access(user_id, server_id, "any_tool", &["developer".into()])
            .await;
        assert!(!allowed, "default-deny: no policy means non-admins denied");
    }

    #[tokio::test]
    async fn default_allows_admin() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        for role in ["admin", "super_admin"] {
            let allowed = ac
                .check_tool_access(user_id, server_id, "any_tool", &[role.into()])
                .await;
            assert!(allowed, "{role} should bypass default-deny");
        }
    }

    #[tokio::test]
    async fn check_by_id_denies_unknown_user() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        // No roles known, no policy set → default deny
        assert!(
            !ac.check_tool_access_by_id(user_id, server_id, "any_tool")
                .await
        );
    }

    #[tokio::test]
    async fn set_policy_denies_unauthorized_role() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        ac.set_policy(server_id, "dangerous_tool".into(), vec!["admin".into()])
            .await;

        let allowed = ac
            .check_tool_access(user_id, server_id, "dangerous_tool", &["developer".into()])
            .await;
        assert!(
            !allowed,
            "developer should be denied when policy requires admin"
        );
    }

    #[tokio::test]
    async fn set_policy_allows_authorized_role() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        ac.set_policy(server_id, "tool".into(), vec!["admin".into()])
            .await;

        let allowed = ac
            .check_tool_access(user_id, server_id, "tool", &["admin".into()])
            .await;
        assert!(allowed, "admin should be allowed when policy lists admin");
    }

    #[tokio::test]
    async fn super_admin_bypasses_policy() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        // Set policy that allows NO roles (empty vec = deny all except super_admin)
        ac.set_policy(server_id, "locked_tool".into(), vec![]).await;

        let allowed = ac
            .check_tool_access(user_id, server_id, "locked_tool", &["super_admin".into()])
            .await;
        assert!(allowed, "super_admin must always have access");
    }

    #[tokio::test]
    async fn empty_policy_denies_non_admin() {
        let ac = AccessController::new();
        let user_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();

        // Empty allowed_roles = explicitly deny everyone except the
        // hardcoded admin / super_admin bypass roles.
        ac.set_policy(server_id, "locked_tool".into(), vec![]).await;

        let allowed = ac
            .check_tool_access(user_id, server_id, "locked_tool", &["developer".into()])
            .await;
        assert!(
            !allowed,
            "empty allowed_roles should deny anyone who isn't admin/super_admin"
        );
    }
}
