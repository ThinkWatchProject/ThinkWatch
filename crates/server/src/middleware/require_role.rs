// ============================================================================
// This file is intentionally minimal after the RBAC cutover.
//
// Authorization is now performed *inline* at the top of every admin handler
// via `AuthUser::require_permission("resource:action")`, which reads the
// permission set from the JWT claims. There is no longer a router-level
// middleware that gates entire subtrees by role — the router only enforces
// authentication (`require_auth`) and handlers decide the rest.
//
// The file is kept so that re-adding a future layer (e.g. a role-agnostic
// rate limit gate) has a natural home.
// ============================================================================
