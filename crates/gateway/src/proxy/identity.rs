//! Materialize the constraints the auth middleware resolved into the
//! rate-limit rules and budget caps one request is held to.

use uuid::Uuid;

use super::GatewayRequestIdentity;
use think_watch_common::limits::{RequestLimits, Surface};

/// The user's limits (role defaults with the user's overrides) on the
/// user's counters, and the calling key's own limits on its lineage's
/// counters — both apply. A request with no user (never past the auth
/// middleware today) is held to nothing.
pub fn limits_for_ai_gateway(identity: &GatewayRequestIdentity) -> RequestLimits {
    let parse = |s: &Option<String>| s.as_deref().and_then(|s| Uuid::parse_str(s).ok());
    let Some(user_id) = parse(&identity.user_id) else {
        return RequestLimits::default();
    };
    let key =
        parse(&identity.api_key_lineage_id).map(|lineage| (lineage, &identity.key_constraints));
    RequestLimits::for_request(
        Surface::AiGateway,
        user_id,
        &identity.surface_constraints,
        key,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use think_watch_common::limits::{
        RateLimitSubject, RateMetric, SurfaceBlock, SurfaceConstraints, SurfaceRule,
    };

    fn one_rule(max_count: i64) -> SurfaceConstraints {
        SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count,
                    enabled: true,
                }],
                budgets: vec![],
            }),
            mcp_gateway: None,
        }
    }

    #[test]
    fn a_keys_rules_count_on_its_lineage_and_its_owners_on_the_user() {
        let user = Uuid::new_v4();
        let lineage = Uuid::new_v4();
        let identity = GatewayRequestIdentity {
            user_id: Some(user.to_string()),
            api_key_id: Some(Uuid::new_v4().to_string()),
            api_key_lineage_id: Some(lineage.to_string()),
            surface_constraints: one_rule(10),
            key_constraints: one_rule(2),
            ..Default::default()
        };
        let limits = limits_for_ai_gateway(&identity);
        assert_eq!(limits.owner, user);
        let subjects: Vec<_> = limits
            .rules
            .iter()
            .map(|r| (r.subject_kind, r.subject_id, r.max_count))
            .collect();
        assert_eq!(
            subjects,
            vec![
                (RateLimitSubject::User, user, 10),
                (RateLimitSubject::ApiKeyLineage, lineage, 2),
            ]
        );
    }

    #[test]
    fn no_user_no_limits() {
        let identity = GatewayRequestIdentity {
            surface_constraints: one_rule(1),
            ..Default::default()
        };
        let limits = limits_for_ai_gateway(&identity);
        assert!(limits.rules.is_empty() && limits.caps.is_empty());
    }
}
