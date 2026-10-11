import { useMemo } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { api } from '@/lib/api';
import { fetchAllPaginated } from '@/lib/paginated-fetch';
import type { ModelsByProvider, McpToolsByServer } from '@/components/roles/PermissionTree';

interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
}

interface McpToolRow {
  id: string;
  server_name: string;
  name: string;
  namespaced_name: string;
}

interface PolicyScope {
  allowed_models: string[] | null;
  allowed_mcp_tools: string[] | null;
}

/// True when `id` is covered by the caller's role-granted `allowed` list.
/// Mirrors the gateway's `request.model == g || request.model.starts_with(g)`
/// so the picker only surfaces models the eventual request would accept.
function modelAllowed(id: string, allowed: string[] | null): boolean {
  if (allowed === null) return true;
  return allowed.some((g) => id === g || id.startsWith(g));
}

/// Mirrors the MCP access-control pattern grammar from
/// `crates/mcp-gateway/src/access_control.rs`: `*`, `<server>__*`, or
/// `<server>__<tool>`.
function mcpToolAllowed(key: string, allowed: string[] | null): boolean {
  if (allowed === null) return true;
  return allowed.some((p) => {
    if (p === '*' || p === key) return true;
    if (p.endsWith('__*')) {
      const prefix = p.slice(0, -'__*'.length);
      return key.startsWith(prefix + '__');
    }
    return false;
  });
}

const NO_COST_CENTERS: string[] = [];
const NO_MODELS: ModelRow[] = [];
const NO_MCP_TOOLS: McpToolRow[] = [];
const UNRESTRICTED_SCOPE: PolicyScope = { allowed_models: null, allowed_mcp_tools: null };

/// What the create and edit key dialogs pick from: the models and MCP
/// tools the caller's roles grant, and the cost centers in use. Loaded
/// only while `enabled`, so a page that opens a dialog now and then
/// fetches the catalogs when it does.
export function useKeyDialogCatalogs(enabled: boolean) {
  const queryClient = useQueryClient();

  // A catalog that fails to load leaves its picker empty — or, for the
  // policy scope, unrestricted.
  const costCentersQuery = useQuery({
    queryKey: ['keys', 'cost-centers'],
    queryFn: ({ signal }) => api<string[]>('/api/keys/cost-centers', { signal }),
    enabled,
  });
  // /api/admin/models and /api/mcp/tools are both paginated and clamp
  // page_size to 200; the dialog pickers want the complete catalog so
  // they can be filtered down to the caller's policy scope locally.
  const modelsQuery = useQuery({
    queryKey: ['admin', 'models', { all: true }],
    queryFn: () => fetchAllPaginated<ModelRow>('/api/admin/models'),
    enabled,
  });
  // `include_user_tools=true` unions the system catalog (`mcp_tools`)
  // with this caller's per-user catalog (`mcp_user_tools` rows where
  // user_id = self), so an admin can grant their own GitHub /
  // Atlassian / Feishu personal tools to an API key. Without this
  // flag the picker would be empty for auth-required servers, since
  // those servers never populate the system-level table by design.
  // (serde's bool deserializer requires `true`/`false` — `1`/`0` 400s.)
  const mcpToolsQuery = useQuery({
    queryKey: ['mcp', 'tools', { all: true, include_user_tools: true }],
    queryFn: () => fetchAllPaginated<McpToolRow>('/api/mcp/tools?include_user_tools=true'),
    enabled,
  });
  const policyScopeQuery = useQuery({
    queryKey: ['keys', 'policy-scope'],
    queryFn: ({ signal }) => api<PolicyScope>('/api/keys/policy-scope', { signal }),
    enabled,
  });
  const costCenterOptions = costCentersQuery.data ?? NO_COST_CENTERS;
  // Full catalog — filtered down to the caller's policy scope below
  // before it reaches the dialog pickers.
  const availableModels = modelsQuery.data ?? NO_MODELS;
  const availableMcpTools = mcpToolsQuery.data ?? NO_MCP_TOOLS;
  const policyScope = policyScopeQuery.data ?? UNRESTRICTED_SCOPE;

  // Shape + filter the raw catalogs into the maps the scope dropdowns
  // consume. `allowed_models === null` means the caller's roles grant
  // everything, so the picker shows the full catalog.
  const modelsByProvider = useMemo<ModelsByProvider>(() => {
    const bucket = availableModels
      .filter((m) => modelAllowed(m.model_id, policyScope.allowed_models))
      .map((m) => ({ modelId: m.model_id, displayName: m.display_name }));
    const out = new Map<string, { modelId: string; displayName: string }[]>();
    if (bucket.length > 0) out.set('', bucket);
    return out;
  }, [availableModels, policyScope.allowed_models]);

  const mcpToolsByServer = useMemo<McpToolsByServer>(() => {
    const out: McpToolsByServer = new Map();
    for (const tool of availableMcpTools) {
      if (!mcpToolAllowed(tool.namespaced_name, policyScope.allowed_mcp_tools)) {
        continue;
      }
      // namespaced_name is `<prefix>__<tool_name>` (tool_name itself may
      // contain single underscores but never `__`).
      const sep = tool.namespaced_name.indexOf('__');
      const prefix = sep >= 0 ? tool.namespaced_name.slice(0, sep) : '';
      const entry = out.get(tool.server_name) ?? {
        serverName: tool.server_name,
        prefix,
        tools: [] as { key: string; toolName: string }[],
      };
      entry.tools.push({ key: tool.namespaced_name, toolName: tool.name });
      out.set(tool.server_name, entry);
    }
    return out;
  }, [availableMcpTools, policyScope.allowed_mcp_tools]);

  const handleCostCenterAdded = (tag: string) => {
    // A new tag reaches the server only with the key that carries it; list
    // it now so the dialog can select it before then.
    queryClient.setQueryData<string[]>(['keys', 'cost-centers'], (prev = []) =>
      prev.includes(tag) ? prev : [...prev, tag].sort(),
    );
  };

  return { modelsByProvider, mcpToolsByServer, costCenterOptions, handleCostCenterAdded };
}
