//! Actor-shaped builders for [`AuditEntry`].
//!
//! Every audit emission has an "actor" with attribution requirements:
//!   - Authenticated handler  → user_id + email + ip + user_agent
//!   - Pre-auth login attempt → email + ip + user_agent (no user_id yet)
//!   - OAuth callback         → user_id (from state cookie) + ip + ua
//!   - System / background    → no actor (action only)
//!   - Gateway request        → user_id + email + api_key_id + ip
//!
//! Without this trait, every handler reaches for `AuditEntry::new(action)`
//! and remembers to attach the right fields. Across ~40 emission sites
//! that "remember" leaks routinely — a class of recurring bugs across
//! six review passes. The trait moves attribution to one declaration
//! per actor type: `actor.audit(action)` is the only path for every
//! non-test caller, and the right fields land by construction.
//!
//! `AuditEntry::new` stays public for the synthesizing-in-tests path,
//! but is marked `#[doc(hidden)]` so production handlers find the
//! trait API first.

use uuid::Uuid;

use super::types::{AuditEntry, LogType};

pub trait AuditActor {
    /// Build an audit entry with actor attribution prefilled. Caller
    /// chains `.resource(...)`, `.detail(...)`, etc. on the returned
    /// builder, then logs via `AuditLogger::log`.
    fn audit(&self, action: impl Into<String>) -> AuditEntry;
}

/// Pre-authentication request actor — login attempts, TOTP steps,
/// registration, password-reset request. IP and user_agent come
/// from the request; email and user_id are filled when the caller
/// has identified the actor:
///   - `auth.login_failed` (email known, user_id not resolved):
///     set email only
///   - `auth.totp_failed` (credentials passed, TOTP step failed):
///     set both email AND user_id — the actor is identified at
///     this point, just hasn't completed all factors
///   - POW challenge mint (truly anonymous): leave email + user_id None
pub struct AnonymousActor<'a> {
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub user_email: Option<&'a str>,
    pub user_id: Option<Uuid>,
}

impl AuditActor for AnonymousActor<'_> {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        // Actor impls legitimately call the bare constructor — it's
        // the only path to a fresh AuditEntry. `#[allow(deprecated)]`
        // is the explicit opt-out the doc on `::new` calls out.
        #[allow(deprecated)]
        let mut e = AuditEntry::new(action);
        if let Some(uid) = self.user_id {
            e = e.user_id(uid);
        }
        if let Some(ip) = self.ip {
            e = e.ip_address(ip);
        }
        if let Some(ua) = self.user_agent {
            e = e.user_agent(ua);
        }
        if let Some(em) = self.user_email {
            e = e.user_email(em);
        }
        e
    }
}

/// OAuth callback actor — user_id is known (resolved from the state
/// cookie before the OAuth provider redirected back), but there's no
/// `AuthUser` extractor because this endpoint runs without a JWT.
pub struct OAuthCallbackActor<'a> {
    pub user_id: Uuid,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

impl AuditActor for OAuthCallbackActor<'_> {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        #[allow(deprecated)]
        let mut e = AuditEntry::new(action).user_id(self.user_id);
        if let Some(ip) = self.ip {
            e = e.ip_address(ip);
        }
        if let Some(ua) = self.user_agent {
            e = e.user_agent(ua);
        }
        e
    }
}

/// System / background-task actor — startup hooks, scheduled cleanup,
/// data-retention sweeps. No human caller, so no actor attribution
/// fields. Distinct from "we forgot to attribute" via the explicit
/// type — `grep AuditEntry::new` in production code becomes a strong
/// signal that someone bypassed the discipline.
pub struct SystemActor;

impl AuditActor for SystemActor {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        #[allow(deprecated)]
        AuditEntry::new(action)
    }
}

/// Gateway request actor — used for every `gateway_logs` row plus
/// the `budget.threshold_crossed` audit on the request path. Doesn't
/// depend on the server crate's `AuthUser` so the gateway crate can
/// construct it directly from `GatewayRequestIdentity`. `.audit()`
/// sets `LogType::Gateway` so callers don't have to chain it.
pub struct GatewayActor<'a> {
    pub user_id: Option<&'a str>,
    pub user_email: Option<&'a str>,
    pub api_key_id: Option<&'a str>,
    pub api_key_lineage_id: Option<&'a str>,
    pub ip: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

impl AuditActor for GatewayActor<'_> {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        // user_id / api_key_id / lineage_id are stored as Uuid in
        // AuditEntry; the gateway's identity carries them as strings
        // (they came off the wire that way). Parse on the way in —
        // a malformed string drops the field rather than corrupting
        // the row.
        #[allow(deprecated)]
        let mut e = AuditEntry::new(action).log_type(LogType::Gateway);
        if let Some(uid) = self.user_id
            && let Ok(u) = uid.parse::<Uuid>()
        {
            e = e.user_id(u);
        }
        if let Some(em) = self.user_email {
            e = e.user_email(em);
        }
        if let Some(kid) = self.api_key_id
            && let Ok(k) = kid.parse::<Uuid>()
        {
            e = e.api_key_id(k);
        }
        if let Some(lid) = self.api_key_lineage_id
            && let Ok(l) = lid.parse::<Uuid>()
        {
            e = e.api_key_lineage_id(l);
        }
        if let Some(ip) = self.ip {
            e = e.ip_address(ip);
        }
        if let Some(sid) = self.session_id {
            e = e.session_id(sid);
        }
        e
    }
}

/// MCP gateway request actor — same role as `GatewayActor` for the
/// MCP path. Distinct from `GatewayActor` because the MCP gateway's
/// identity carries `user_id` as a typed `Uuid` (not a string),
/// `LogType::Mcp` is what `.audit()` sets, and MCP doesn't model
/// api_key_id / session_id / lineage at the request log layer.
pub struct McpActor<'a> {
    pub user_id: Uuid,
    pub user_email: &'a str,
    pub ip: Option<&'a str>,
}

impl AuditActor for McpActor<'_> {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        #[allow(deprecated)]
        let mut e = AuditEntry::new(action)
            .log_type(LogType::Mcp)
            .user_id(self.user_id)
            .user_email(self.user_email);
        if let Some(ip) = self.ip {
            e = e.ip_address(ip);
        }
        e
    }
}

#[cfg(test)]
mod tests {
    //! Actor impls are the foundation the whole audit-attribution
    //! discipline rests on. A refactor that reorders if-let chains or
    //! forgets a field would silently drop forensic context on every
    //! emitted row. Pin each actor's contract here so the next change
    //! gets a test signal.

    use super::*;

    #[test]
    fn anonymous_actor_populates_all_present_fields() {
        let uid = Uuid::new_v4();
        let actor = AnonymousActor {
            ip: Some("203.0.113.7"),
            user_agent: Some("Mozilla/5.0"),
            user_email: Some("alice@example.com"),
            user_id: Some(uid),
        };
        let e = actor.audit("auth.login_failed");
        assert_eq!(e.action, "auth.login_failed");
        assert_eq!(e.ip_address.as_deref(), Some("203.0.113.7"));
        assert_eq!(e.user_agent.as_deref(), Some("Mozilla/5.0"));
        assert_eq!(e.user_email.as_deref(), Some("alice@example.com"));
        assert_eq!(e.user_id.as_deref(), Some(uid.to_string().as_str()));
        assert!(matches!(e.log_type, LogType::Audit));
    }

    #[test]
    fn anonymous_actor_truly_anonymous_omits_optional_fields() {
        // POW challenge mint case — only IP is known.
        let actor = AnonymousActor {
            ip: Some("203.0.113.7"),
            user_agent: None,
            user_email: None,
            user_id: None,
        };
        let e = actor.audit("auth.pow_challenge");
        assert_eq!(e.ip_address.as_deref(), Some("203.0.113.7"));
        assert!(e.user_agent.is_none());
        assert!(e.user_email.is_none());
        assert!(e.user_id.is_none());
    }

    #[test]
    fn oauth_callback_actor_user_id_is_mandatory_and_log_type_is_audit() {
        let uid = Uuid::new_v4();
        let actor = OAuthCallbackActor {
            user_id: uid,
            ip: None,
            user_agent: None,
        };
        let e = actor.audit("mcp.connection.authorized");
        assert_eq!(e.user_id.as_deref(), Some(uid.to_string().as_str()));
        assert!(matches!(e.log_type, LogType::Audit));
    }

    #[test]
    fn system_actor_carries_no_attribution() {
        let e = SystemActor.audit("data.gdpr_purge");
        assert!(e.user_id.is_none());
        assert!(e.user_email.is_none());
        assert!(e.ip_address.is_none());
        assert!(e.user_agent.is_none());
        assert!(e.api_key_id.is_none());
    }

    #[test]
    fn gateway_actor_sets_log_type_gateway_and_parses_uuid_strings() {
        let uid = Uuid::new_v4().to_string();
        let kid = Uuid::new_v4().to_string();
        let lid = Uuid::new_v4().to_string();
        let actor = GatewayActor {
            user_id: Some(&uid),
            user_email: Some("alice@example.com"),
            api_key_id: Some(&kid),
            api_key_lineage_id: Some(&lid),
            ip: Some("203.0.113.7"),
            session_id: Some("sess-abc"),
        };
        let e = actor.audit("chat.completion");
        assert!(matches!(e.log_type, LogType::Gateway));
        assert_eq!(e.user_id.as_deref(), Some(uid.as_str()));
        assert_eq!(e.api_key_id.as_deref(), Some(kid.as_str()));
        assert_eq!(e.api_key_lineage_id.as_deref(), Some(lid.as_str()));
        assert_eq!(e.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(e.ip_address.as_deref(), Some("203.0.113.7"));
    }

    #[test]
    fn gateway_actor_drops_malformed_uuid_strings_silently() {
        // Mirrors the production "bad string from wire = absent field,
        // not corrupt row" contract — pin so a future refactor that
        // panics on parse failure (or substitutes a zero Uuid) breaks
        // this test. Covers all three Uuid-typed fields the actor
        // parses (user_id / api_key_id / api_key_lineage_id) so a
        // regression on any one of them surfaces here.
        let actor = GatewayActor {
            user_id: Some("not-a-uuid"),
            user_email: None,
            api_key_id: Some("also-not-a-uuid"),
            api_key_lineage_id: Some("definitely-not-a-uuid"),
            ip: None,
            session_id: None,
        };
        let e = actor.audit("chat.completion");
        assert!(e.user_id.is_none());
        assert!(e.api_key_id.is_none());
        assert!(e.api_key_lineage_id.is_none());
    }

    #[test]
    fn gateway_actor_with_no_inputs_produces_log_type_only_entry() {
        // Floor case — the migrated emit_* paths hit this when a
        // request lands entirely without identity (untyped surface,
        // probe, etc). Verify no field is invented and `log_type`
        // is the actor's declared default.
        let actor = GatewayActor {
            user_id: None,
            user_email: None,
            api_key_id: None,
            api_key_lineage_id: None,
            ip: None,
            session_id: None,
        };
        let e = actor.audit("chat.completion");
        assert_eq!(e.action, "chat.completion");
        assert!(matches!(e.log_type, LogType::Gateway));
        assert!(e.user_id.is_none());
        assert!(e.user_email.is_none());
        assert!(e.api_key_id.is_none());
        assert!(e.api_key_lineage_id.is_none());
        assert!(e.ip_address.is_none());
        assert!(e.user_agent.is_none());
        assert!(e.session_id.is_none());
    }

    #[test]
    fn mcp_actor_sets_log_type_mcp() {
        let uid = Uuid::new_v4();
        let actor = McpActor {
            user_id: uid,
            user_email: "alice@example.com",
            ip: Some("203.0.113.7"),
        };
        let e = actor.audit("tools.call");
        assert!(matches!(e.log_type, LogType::Mcp));
        assert_eq!(e.user_id.as_deref(), Some(uid.to_string().as_str()));
        assert_eq!(e.user_email.as_deref(), Some("alice@example.com"));
        assert_eq!(e.ip_address.as_deref(), Some("203.0.113.7"));
    }
}
