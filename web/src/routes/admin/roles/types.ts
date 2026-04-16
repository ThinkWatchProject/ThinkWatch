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
  policy_document: PolicyDocument;
  user_count: number;
  /** Email of the user who created this role. `null` for seeded
   *  system roles or roles whose creator was deleted. */
  created_by_email: string | null;
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

export interface PolicyConstraints {
  RateLimits?: { Metric: string; Window: string; MaxCount: number }[];
  Budgets?: { Period: string; MaxTokens: number }[];
}

export interface PolicyStatement {
  Sid?: string;
  Effect: 'Allow' | 'Deny';
  Action: string | string[];
  Resource: string | string[];
  Constraints?: PolicyConstraints;
}

export interface McpServer {
  id: string;
  name: string;
  /** Short identifier used as the tool-namespace prefix. Tools are
   *  exposed as `<namespace_prefix>__<tool_name>` in ACLs. */
  namespace_prefix: string;
}

export interface McpToolRow {
  id: string;
  server_id: string;
  /** Display name of the server this tool belongs to. */
  server_name: string;
  /** Raw tool name from the upstream MCP server (e.g. `list_issues`). */
  name: string;
  /** Fully namespaced id used in ACLs (e.g. `github__list_issues`). */
  namespaced_name: string;
  description: string | null;
}

export interface ModelRow {
  id: string;
  provider_id: string;
  provider_name: string;
  model_id: string;
  display_name: string;
  is_active: boolean;
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
  /// Optional model-id allowlist preset. `undefined` = unrestricted
  /// (template doesn't constrain models). Empty array = explicit
  /// "no models reachable" — useful for read-only roles that shouldn't
  /// invoke any model even if granted ai_gateway:use.
  models?: string[];
  /// Optional MCP-tool allowlist preset (matches `mcpToolKey` patterns
  /// like `mysql__*` or `github__list_issues`).
  mcpTools?: string[];
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
  // Models / MCP tools are explicitly empty so a read-only viewer can't
  // accidentally rack up usage on someone else's quota.
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
    models: [],
    mcpTools: [],
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
// Parsed constraints — camelCase TS representation of the PascalCase JSON
// Constraints blocks attached to individual Statements.
// ----------------------------------------------------------------------------

export interface ParsedRateLimit {
  metric: 'requests' | 'tokens';
  window: string;
  maxCount: number;
  enabled: boolean;
}

export interface ParsedBudget {
  period: 'daily' | 'weekly' | 'monthly';
  maxTokens: number;
  enabled: boolean;
}

export interface ParsedSurfaceConstraints {
  rateLimits: ParsedRateLimit[];
  budgets: ParsedBudget[];
}

export interface ParsedConstraints {
  ai_gateway?: ParsedSurfaceConstraints;
  mcp_gateway?: ParsedSurfaceConstraints;
}

// ----------------------------------------------------------------------------
// Simple ↔ Policy mode conversion
//
// `permsToPolicy` groups permissions by surface (ai_gateway, mcp_gateway,
// rest) and builds one Statement per group. Constraints (rate limits,
// budgets) are attached to the relevant Statement rather than as a
// top-level key.
//
// `policyToPerms` walks every Statement and harvests Action keys, model/tool
// scope from Resource, and constraints from each Statement's Constraints.
// ----------------------------------------------------------------------------

function constraintsToJson(c: ParsedSurfaceConstraints): PolicyConstraints | undefined {
  const out: PolicyConstraints = {};
  // Only serialize enabled rules/budgets into the policy document
  const enabledRules = c.rateLimits.filter((r) => r.enabled);
  const enabledBudgets = c.budgets.filter((b) => b.enabled);
  if (enabledRules.length > 0) {
    out.RateLimits = enabledRules.map((r) => ({
      Metric: r.metric,
      Window: r.window,
      MaxCount: r.maxCount,
    }));
  }
  if (enabledBudgets.length > 0) {
    out.Budgets = enabledBudgets.map((b) => ({
      Period: b.period,
      MaxTokens: b.maxTokens,
    }));
  }
  return out.RateLimits || out.Budgets ? out : undefined;
}

function constraintsFromJson(c: PolicyConstraints | undefined): ParsedSurfaceConstraints {
  return {
    rateLimits: (c?.RateLimits ?? []).map((r) => ({
      metric: r.Metric as 'requests' | 'tokens',
      window: r.Window,
      maxCount: r.MaxCount,
      enabled: true,
    })),
    budgets: (c?.Budgets ?? []).map((b) => ({
      period: b.Period as 'daily' | 'weekly' | 'monthly',
      maxTokens: b.MaxTokens,
      enabled: true,
    })),
  };
}

function isEmptySurfaceConstraints(c: ParsedSurfaceConstraints): boolean {
  return c.rateLimits.length === 0 && c.budgets.length === 0;
}

/// Encode the simple-mode form into a policy document. Permissions are
/// grouped by surface: ai_gateway perms get their own Statement with
/// model scope as Resource and AI constraints; mcp_gateway perms get
/// their own Statement with tool scope and MCP constraints; remaining
/// perms go into a catch-all Statement.
export function permsToPolicy(
  perms: Set<string>,
  models?: Set<string> | null,
  mcpTools?: Set<string> | null,
  constraints?: ParsedConstraints | null,
): PolicyDocument {
  const statements: PolicyStatement[] = [];

  const aiPerms = [...perms].filter((p) => p.startsWith('ai_gateway:')).sort();
  const mcpPerms = [...perms].filter((p) => p.startsWith('mcp_gateway:')).sort();
  const restPerms = [...perms]
    .filter((p) => !p.startsWith('ai_gateway:') && !p.startsWith('mcp_gateway:'))
    .sort();

  if (aiPerms.length > 0 || models != null) {
    const resource: string | string[] =
      models == null
        ? '*'
        : models.size === 0
          ? []
          : Array.from(models)
              .sort()
              .map((m) => `model:${m}`);
    const aiConstraints = constraints?.ai_gateway;
    const stmt: PolicyStatement = {
      Sid: 'AIGateway',
      Effect: 'Allow',
      Action: aiPerms.length > 0 ? aiPerms : 'ai_gateway:use',
      Resource: resource,
    };
    if (aiConstraints && !isEmptySurfaceConstraints(aiConstraints)) {
      stmt.Constraints = constraintsToJson(aiConstraints);
    }
    statements.push(stmt);
  }

  if (mcpPerms.length > 0 || mcpTools != null) {
    const resource: string | string[] =
      mcpTools == null
        ? '*'
        : mcpTools.size === 0
          ? []
          : Array.from(mcpTools)
              .sort()
              .map((t) => `mcp_tool:${t}`);
    const mcpConstraints = constraints?.mcp_gateway;
    const stmt: PolicyStatement = {
      Sid: 'MCPGateway',
      Effect: 'Allow',
      Action: mcpPerms.length > 0 ? mcpPerms : 'mcp_gateway:use',
      Resource: resource,
    };
    if (mcpConstraints && !isEmptySurfaceConstraints(mcpConstraints)) {
      stmt.Constraints = constraintsToJson(mcpConstraints);
    }
    statements.push(stmt);
  }

  if (restPerms.length > 0) {
    statements.push({
      Sid: 'ConsoleAccess',
      Effect: 'Allow',
      Action: restPerms,
      Resource: '*',
    });
  }

  return { Version: '2024-01-01', Statement: statements };
}

export function isWildcardResource(r: PolicyStatement['Resource']): boolean {
  if (r === '*') return true;
  if (Array.isArray(r)) return r.includes('*');
  return false;
}

export interface PolicyParseResult {
  perms: Set<string>;
  /** `null` = unrestricted (no model-scoped Resource seen); Set = restrict. */
  models: Set<string> | null;
  /** Same shape, for MCP tools scope. */
  mcpTools: Set<string> | null;
  /** Constraints extracted from per-Statement Constraints blocks, grouped by surface. */
  constraints: ParsedConstraints;
  /** True if any statement couldn't be expressed in simple mode. */
  lossy: boolean;
  parseError: boolean;
}

export function policyToPerms(json: string, available: PermissionDef[]): PolicyParseResult {
  const out: PolicyParseResult = {
    perms: new Set(),
    models: null,
    mcpTools: null,
    constraints: {},
    lossy: false,
    parseError: false,
  };
  if (!json.trim()) return out;
  let doc: PolicyDocument;
  try {
    doc = JSON.parse(json) as PolicyDocument;
  } catch {
    out.parseError = true;
    return out;
  }
  const valid = new Set(available.map((p) => p.key));
  for (const st of doc.Statement ?? []) {
    if (st.Effect !== 'Allow') {
      out.lossy = true;
      continue;
    }
    const resources = Array.isArray(st.Resource) ? st.Resource : [st.Resource];
    const modelResources = resources.filter(
      (r): r is string => typeof r === 'string' && r.startsWith('model:'),
    );
    const toolResources = resources.filter(
      (r): r is string => typeof r === 'string' && r.startsWith('mcp_tool:'),
    );
    const actions = Array.isArray(st.Action) ? st.Action : [st.Action];
    const hasAiAction = actions.some((a) => a.startsWith('ai_gateway:'));
    const hasMcpAction = actions.some((a) => a.startsWith('mcp_gateway:'));

    // Extract model scope from Resource
    if (modelResources.length > 0) {
      out.models = new Set(modelResources.map((r) => r.slice('model:'.length)));
    } else if (
      hasAiAction &&
      !isWildcardResource(st.Resource) &&
      resources.length === 0
    ) {
      // Empty resource array = explicit "no models allowed"
      out.models = new Set();
    }

    // Extract tool scope from Resource
    if (toolResources.length > 0) {
      out.mcpTools = new Set(toolResources.map((r) => r.slice('mcp_tool:'.length)));
    } else if (
      hasMcpAction &&
      !isWildcardResource(st.Resource) &&
      resources.length === 0
    ) {
      out.mcpTools = new Set();
    }

    // Extract constraints by surface
    if (st.Constraints) {
      const parsed = constraintsFromJson(st.Constraints);
      if (hasAiAction) {
        out.constraints.ai_gateway = parsed;
      } else if (hasMcpAction) {
        out.constraints.mcp_gateway = parsed;
      }
    }

    // Collect Action keys as permissions
    if (isWildcardResource(st.Resource) || modelResources.length > 0 || toolResources.length > 0) {
      for (const a of actions) {
        if (a === '*') {
          for (const p of available) out.perms.add(p.key);
        } else if (a.endsWith(':*')) {
          const prefix = a.slice(0, -1);
          for (const p of available) if (p.key.startsWith(prefix)) out.perms.add(p.key);
        } else if (valid.has(a)) {
          out.perms.add(a);
        } else {
          out.lossy = true;
        }
      }
    } else if (!isWildcardResource(st.Resource) && modelResources.length === 0 && toolResources.length === 0) {
      // Non-wildcard Resource without model:/mcp_tool: prefix
      out.lossy = true;
    }
  }
  return out;
}
