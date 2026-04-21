//! SCIM 2.0 — automatic user / group provisioning from upstream IdPs
//! (Okta, Azure AD, Google Workspace, Auth0).
//!
//! ## Status
//!
//! Foundation only. The two routes below answer the spec-mandatory
//! `/scim/v2/ServiceProviderConfig` and `/scim/v2/ResourceTypes`
//! discovery endpoints so an IdP can probe the connection during
//! setup; the actual `/Users` and `/Groups` CRUD lives in subsequent
//! work.
//!
//! ## Roadmap
//!
//! 1. `GET /scim/v2/Users` — list with `filter` param
//! 2. `GET /scim/v2/Users/{id}`
//! 3. `POST /scim/v2/Users` — provision new user
//! 4. `PATCH /scim/v2/Users/{id}` — update / disable
//! 5. `DELETE /scim/v2/Users/{id}` — soft-delete (we keep audit
//!    history; SCIM expects 204)
//! 6. `GET /scim/v2/Groups` — `team_role_assignments` translation
//! 7. `POST /scim/v2/Groups/{id}/members` — assign user → team
//!
//! ## Auth
//!
//! Bearer token from a dedicated SCIM key. SCIM clients sit outside
//! the normal session cookie / signed-request envelope, so they get
//! their own surface (`api_key.surfaces` gains `'scim'`) — that
//! avoids opening JWT auth to a non-browser actor.
//!
//! ## Data shape
//!
//! IdP-supplied fields are stored straight onto `users` (email,
//! display_name) and `team_members` (group → team mapping). The
//! IdP is the source of truth for membership; deactivation arrives
//! as `active=false` and we forward to the existing
//! `force-logout` + soft-delete path so audit invariants hold.

use axum::Json;
use serde_json::json;

/// `GET /scim/v2/ServiceProviderConfig` — capability discovery.
/// Spec: RFC 7643 §5.
#[allow(dead_code)] // staged for future routing wiring
pub async fn service_provider_config() -> Json<serde_json::Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "documentationUri": "https://datatracker.ietf.org/doc/html/rfc7643",
        "patch":     { "supported": true },
        "bulk":      { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter":    { "supported": true, "maxResults": 200 },
        "changePassword": { "supported": false },
        "sort":      { "supported": false },
        "etag":      { "supported": false },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "Bearer Token",
            "description": "OAuth2 bearer using a tw- API key with the `scim` surface",
            "primary": true
        }]
    }))
}

/// `GET /scim/v2/ResourceTypes` — what we support.
#[allow(dead_code)] // staged for future routing wiring
pub async fn resource_types() -> Json<serde_json::Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "Resources": [
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "User",
                "name": "User",
                "endpoint": "/Users",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:User"
            },
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "Group",
                "name": "Group",
                "endpoint": "/Groups",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:Group"
            }
        ]
    }))
}
