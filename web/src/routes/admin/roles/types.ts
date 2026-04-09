// Types, constants, and pure helpers for the Roles page.
//
// Extracted out of the main roles.tsx so the page file is large
// instead of unmanageable. The page component still owns all
// stateful behavior (forms, dialogs, member lists, history tabs);
// this file holds the data shapes, the policy templates, and the
// simple↔policy conversion logic that's pure and unit-testable.

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

export interface PermissionDef {
  key: string;
  resource: string;
  action: string;
  dangerous: boolean;
}

export interface RoleResponse {
  id: string;
  name: string;
  description: string | null;
  is_system: boolean;
  permissions: string[];
  allowed_models: string[] | null;
  allowed_mcp_servers: string[] | null;
  policy_document: PolicyDocument | null;
  user_count: number;
  created_at: string;
  updated_at: string;
}

export interface RoleMember {
  user_id: string;
  email: string;
  display_name: string | null;
  scope: string;
  source: 'system' | 'custom';
  assigned_at: string | null;
}

/// One audit-log row scoped to a single role. Sourced from
/// `platform_logs` via `GET /api/admin/roles/:id/history`.
export interface RoleHistoryEntry {
  id: string;
  action: string;
  user_id: string | null;
  user_email: string | null;
  ip_address: string | null;
  detail: Record<string, unknown> | null;
  created_at: string;
}

export interface PolicyDocument {
  Version: string;
  Statement: PolicyStatement[];
}

export interface PolicyStatement {
  Sid?: string;
  Effect: 'Allow' | 'Deny';
  Action: string | string[];
  Resource: string | string[];
  Condition?: Record<string, unknown>;
}

export interface McpServer {
  id: string;
  name: string;
}

// ----------------------------------------------------------------------------
// Policy templates — drop-in starters for the structured policy mode.
// ----------------------------------------------------------------------------

export const POLICY_TEMPLATES: Record<string, PolicyDocument> = {
  fullAccess: {
    Version: '2024-01-01',
    Statement: [{ Sid: 'FullAccess', Effect: 'Allow', Action: '*', Resource: '*' }],
  },
  readOnly: {
    Version: '2024-01-01',
    Statement: [
      {
        Sid: 'ReadOnly',
        Effect: 'Allow',
        Action: ['analytics:read_own', 'api_keys:read', 'providers:read', 'mcp_servers:read'],
        Resource: '*',
      },
      { Sid: 'DenyWrite', Effect: 'Deny', Action: '*:write', Resource: '*' },
    ],
  },
  developer: {
    Version: '2024-01-01',
    Statement: [
      {
        Sid: 'AllowGateway',
        Effect: 'Allow',
        Action: ['ai_gateway:use', 'mcp_gateway:use'],
        Resource: '*',
      },
      {
        Sid: 'AllowKeys',
        Effect: 'Allow',
        Action: ['api_keys:read', 'api_keys:create', 'api_keys:update'],
        Resource: '*',
      },
      { Sid: 'AllowAnalytics', Effect: 'Allow', Action: 'analytics:read_own', Resource: '*' },
    ],
  },
  gatewayOnly: {
    Version: '2024-01-01',
    Statement: [
      {
        Sid: 'GatewayOnly',
        Effect: 'Allow',
        Action: ['ai_gateway:use', 'mcp_gateway:use'],
        Resource: '*',
      },
    ],
  },
  modelRestricted: {
    Version: '2024-01-01',
    Statement: [
      {
        Sid: 'AllowGateway',
        Effect: 'Allow',
        Action: 'ai_gateway:use',
        Resource: ['model:gpt-4o', 'model:gpt-4o-mini'],
      },
      {
        Sid: 'DenyOtherModels',
        Effect: 'Deny',
        Action: 'ai_gateway:use',
        Resource: 'model:*',
      },
    ],
  },
};

// ----------------------------------------------------------------------------
// Simple-mode starter templates
//
// Each template is a curated set of permission keys the admin can drop
// into the form with one click. The clone-from-existing picker handles
// the "fork an existing role" workflow; this list covers the
// from-scratch shapes that are common enough to deserve a shortcut.
// ----------------------------------------------------------------------------

export interface SimpleTemplate {
  id: 'gateway_user' | 'read_only' | 'ops_admin' | 'analytics_only';
  /// Permission keys this template grants. Validated against the live
  /// PERMISSION_CATALOG before being applied — anything not in the
  /// catalog is silently dropped.
  permissions: string[];
}

export const SIMPLE_TEMPLATES: SimpleTemplate[] = [
  // Just-enough permissions to call the AI + MCP gateways and manage
  // own API keys. Mirrors the developer system role.
  {
    id: 'gateway_user',
    permissions: [
      'ai_gateway:use',
      'mcp_gateway:use',
      'api_keys:read',
      'api_keys:create',
      'api_keys:update',
      'providers:read',
      'mcp_servers:read',
      'analytics:read_own',
      'audit_logs:read_own',
      'logs:read_own',
    ],
  },
  // Read-only across the surface a non-admin can browse.
  {
    id: 'read_only',
    permissions: [
      'api_keys:read',
      'providers:read',
      'mcp_servers:read',
      'roles:read',
      'analytics:read_own',
      'audit_logs:read_own',
      'logs:read_own',
      'settings:read',
      'log_forwarders:read',
      'webhooks:read',
      'content_filter:read',
      'pii_redactor:read',
    ],
  },
  // Operational admin that can run the platform but cannot touch
  // users, roles, or system-level OIDC config. Useful for an SRE who
  // should be able to register providers and rotate keys without
  // also being able to grant themselves more access.
  {
    id: 'ops_admin',
    permissions: [
      'ai_gateway:use',
      'mcp_gateway:use',
      'api_keys:read',
      'api_keys:create',
      'api_keys:update',
      'api_keys:rotate',
      'api_keys:delete',
      'providers:read',
      'providers:create',
      'providers:update',
      'providers:rotate_key',
      'mcp_servers:read',
      'mcp_servers:create',
      'mcp_servers:update',
      'mcp_servers:delete',
      'analytics:read_all',
      'audit_logs:read_all',
      'logs:read_all',
      'log_forwarders:read',
      'log_forwarders:write',
      'webhooks:read',
      'webhooks:write',
      'content_filter:read',
      'content_filter:write',
      'pii_redactor:read',
      'pii_redactor:write',
      'settings:read',
    ],
  },
  // Analytics-only viewer (e.g. an SRE dashboard or finance owner).
  {
    id: 'analytics_only',
    permissions: ['analytics:read_all', 'audit_logs:read_all', 'logs:read_all'],
  },
];

// ----------------------------------------------------------------------------
// Permission grouping helpers
// ----------------------------------------------------------------------------

/// Group a flat permission catalog into `{ resource: PermissionDef[] }`.
/// The order is preserved (resources appear in the order they first show up
/// in the catalog) so the UI is stable.
export function groupByResource(perms: PermissionDef[]): Map<string, PermissionDef[]> {
  const out = new Map<string, PermissionDef[]>();
  for (const p of perms) {
    const arr = out.get(p.resource);
    if (arr) arr.push(p);
    else out.set(p.resource, [p]);
  }
  return out;
}

// ----------------------------------------------------------------------------
// Simple ↔ Policy mode conversion
//
// `permsToPolicy` produces a single Allow statement listing every selected
// permission key — round-trips losslessly back through `policyToPerms`.
//
// `policyToPerms` walks every Statement and harvests Action keys from any
// Allow rule whose Resource matches `*` (or `["*"]`). Anything fancier
// (Deny rules, Resource scoping like `model:gpt-*`, conditions) cannot be
// represented in simple mode and is reported as lossy so the UI can warn
// the admin before they overwrite the JSON.
// ----------------------------------------------------------------------------

export function permsToPolicy(perms: Set<string>): PolicyDocument {
  if (perms.size === 0) return { Version: '2024-01-01', Statement: [] };
  return {
    Version: '2024-01-01',
    Statement: [
      {
        Sid: 'AllowPermissions',
        Effect: 'Allow',
        Action: Array.from(perms).sort(),
        Resource: '*',
      },
    ],
  };
}

export function isWildcardResource(r: PolicyStatement['Resource']): boolean {
  if (r === '*') return true;
  if (Array.isArray(r)) return r.includes('*');
  return false;
}

export function policyToPerms(
  json: string,
  available: PermissionDef[],
): { perms: Set<string>; lossy: boolean; parseError: boolean } {
  const out = new Set<string>();
  if (!json.trim()) return { perms: out, lossy: false, parseError: false };
  let doc: PolicyDocument;
  try {
    doc = JSON.parse(json) as PolicyDocument;
  } catch {
    return { perms: out, lossy: false, parseError: true };
  }
  let lossy = false;
  const valid = new Set(available.map((p) => p.key));
  for (const st of doc.Statement ?? []) {
    if (st.Effect !== 'Allow' || !isWildcardResource(st.Resource)) {
      lossy = true;
      continue;
    }
    const actions = Array.isArray(st.Action) ? st.Action : [st.Action];
    for (const a of actions) {
      if (a === '*') {
        for (const p of available) out.add(p.key);
      } else if (a.endsWith(':*')) {
        const prefix = a.slice(0, -1); // includes the colon
        for (const p of available) if (p.key.startsWith(prefix)) out.add(p.key);
      } else if (valid.has(a)) {
        out.add(a);
      } else {
        lossy = true;
      }
    }
  }
  return { perms: out, lossy, parseError: false };
}
