/**
 * Single source of truth for "which permission does each protected
 * route require to view." Consumed by:
 *   - `app-sidebar.tsx` to hide nav items the user can't reach.
 *   - `router.tsx` to gate the route-component mount with
 *     `<RequirePermission perm="…">` so direct-URL access by an
 *     under-privileged user lands on the Forbidden page instead of
 *     a half-rendered tab firing 403s on every API call.
 *
 * `undefined` means "any authenticated user". Public routes (login,
 * setup, register) aren't listed here — they're handled by the auth
 * cookie check, not RBAC.
 *
 * **Adding a new route:** add an entry here AND set up the matching
 * backend `require_permission(...)` in the handler. The route guard
 * is a UX nicety; the backend is the authoritative gate.
 */
export const ROUTE_PERMISSIONS: Record<string, string | undefined> = {
  // --- Overview (any authenticated user) ---
  '/': undefined, // dashboard
  '/api-keys': undefined,
  '/guide': undefined,
  '/profile': undefined,

  // --- AI Gateway ---
  '/gateway/providers': 'providers:read',
  '/gateway/models': 'models:read',
  '/gateway/security': 'content_filter:read',

  // --- MCP Gateway ---
  '/mcp/servers': 'mcp_servers:read',
  '/mcp/tools': 'mcp_servers:read',
  '/mcp/store': 'mcp_servers:read',

  // --- Analytics ---
  '/analytics/usage': 'analytics:read_own',
  '/analytics/costs': 'analytics:read_own',

  // --- Logs ---
  '/logs': 'logs:read_all',
  '/logs/forwarders': 'log_forwarders:read',
  '/admin/trace': 'analytics:read_all',
  '/admin/trace/$traceId': 'analytics:read_all',

  // --- Admin ---
  '/admin/users': 'users:read',
  '/admin/teams': 'teams:read',
  '/admin/teams/$id': 'teams:read',
  '/admin/roles': 'roles:read',
  '/admin/settings': 'settings:read',
  '/admin/api-docs': 'settings:read',
  '/admin/usage-license': 'analytics:read_all',
};

/**
 * Look up the permission required for a given route path. Falls
 * back to `undefined` (any authenticated user) for unrecognised
 * paths so a typo here doesn't lock everyone out by accident.
 */
export function permissionForRoute(href: string): string | undefined {
  return ROUTE_PERMISSIONS[href];
}
