import {
  useEffect,
  useMemo,
  useState,
  useCallback,
  type FormEvent,
  type ReactNode,
} from 'react';
import { useTranslation } from 'react-i18next';
import { Card } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import { Checkbox } from '@/components/ui/checkbox';
import { Textarea } from '@/components/ui/textarea';
import {
  Shield,
  Plus,
  Pencil,
  Trash2,
  Copy,
  FileJson,
  Search,
  AlertTriangle,
  Lock,
} from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { ScrollArea } from '@/components/ui/scroll-area';

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

interface PermissionDef {
  key: string;
  resource: string;
  action: string;
  dangerous: boolean;
}

interface RoleResponse {
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

interface RoleMember {
  user_id: string;
  email: string;
  display_name: string | null;
  scope: string;
  source: 'system' | 'custom';
  assigned_at: string | null;
}

interface PolicyDocument {
  Version: string;
  Statement: PolicyStatement[];
}

interface PolicyStatement {
  Sid?: string;
  Effect: 'Allow' | 'Deny';
  Action: string | string[];
  Resource: string | string[];
  Condition?: Record<string, unknown>;
}

interface McpServer {
  id: string;
  name: string;
}

// ----------------------------------------------------------------------------
// Policy templates (kept; the IAM/policy mode is still useful for power users)
// ----------------------------------------------------------------------------

const POLICY_TEMPLATES: Record<string, PolicyDocument> = {
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

interface SimpleTemplate {
  id: 'gateway_user' | 'read_only' | 'ops_admin' | 'analytics_only';
  /// Permission keys this template grants. Validated against the live
  /// PERMISSION_CATALOG before being applied — anything not in the
  /// catalog is silently dropped.
  permissions: string[];
}

const SIMPLE_TEMPLATES: SimpleTemplate[] = [
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
function groupByResource(perms: PermissionDef[]): Map<string, PermissionDef[]> {
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
function permsToPolicy(perms: Set<string>): PolicyDocument {
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

function isWildcardResource(r: PolicyStatement['Resource']): boolean {
  if (r === '*') return true;
  if (Array.isArray(r)) return r.includes('*');
  return false;
}

function policyToPerms(
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

// ----------------------------------------------------------------------------
// Page
// ----------------------------------------------------------------------------

export function RolesPage() {
  const { t } = useTranslation();
  const [roles, setRoles] = useState<RoleResponse[]>([]);
  const [permissions, setPermissions] = useState<PermissionDef[]>([]);
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [availableServers, setAvailableServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);

  // Filters
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState<'all' | 'system' | 'custom'>('all');

  // Create dialog
  const [createOpen, setCreateOpen] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formPerms, setFormPerms] = useState<Set<string>>(new Set());
  const [formModels, setFormModels] = useState<Set<string>>(new Set());
  const [formServers, setFormServers] = useState<Set<string>>(new Set());
  const [formRestrictModels, setFormRestrictModels] = useState(false);
  const [formRestrictServers, setFormRestrictServers] = useState(false);
  const [formMode, setFormMode] = useState<'simple' | 'policy'>('simple');
  const [formPolicyJson, setFormPolicyJson] = useState('');
  const [formPolicyError, setFormPolicyError] = useState('');
  const [creating, setCreating] = useState(false);

  // Edit dialog
  const [editOpen, setEditOpen] = useState(false);
  const [editRole, setEditRole] = useState<RoleResponse | null>(null);
  const [editName, setEditName] = useState('');
  const [editDesc, setEditDesc] = useState('');
  const [editPerms, setEditPerms] = useState<Set<string>>(new Set());
  const [editModels, setEditModels] = useState<Set<string>>(new Set());
  const [editServers, setEditServers] = useState<Set<string>>(new Set());
  const [editRestrictModels, setEditRestrictModels] = useState(false);
  const [editRestrictServers, setEditRestrictServers] = useState(false);
  const [editMode, setEditMode] = useState<'simple' | 'policy'>('simple');
  const [editPolicyJson, setEditPolicyJson] = useState('');
  const [editPolicyError, setEditPolicyError] = useState('');
  const [saving, setSaving] = useState(false);

  // Detail (read-only inspector for system roles)
  const [detailRole, setDetailRole] = useState<RoleResponse | null>(null);

  // Delete with reassign. Two modes:
  //   - 'bulk': pick a single target role for every current member.
  //     This goes through the existing `?reassign_to=` query string
  //     so the backend handles it atomically.
  //   - 'per_member': admin picks a different target per member, via
  //     N PATCH /api/admin/users/{id} calls before the DELETE. This is
  //     non-atomic but covers the "split into multiple buckets" case
  //     the bulk mode can't express.
  const [deleteRole, setDeleteRole] = useState<RoleResponse | null>(null);
  const [reassignTo, setReassignTo] = useState<string>('');
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string>('');
  const [deleteMode, setDeleteMode] = useState<'bulk' | 'per_member'>('bulk');
  /// Members of the role being deleted, fetched when the dialog
  /// opens. Null while loading.
  const [deleteMembers, setDeleteMembers] = useState<RoleMember[] | null>(null);
  /// Per-member target role mapping. Key is `${user_id}-${scope}` so
  /// distinct scope assignments for the same user can be migrated
  /// to different roles.
  const [perMemberTargets, setPerMemberTargets] = useState<Record<string, string>>({});

  /// Confirmation gate for saving a role with dangerous permissions
  /// selected. The submit handlers stash the actual save action here
  /// instead of running it directly when the danger set is non-empty.
  /// `keys` is the resolved set of dangerous keys we'll show in the
  /// confirm dialog so the admin sees exactly what they're approving.
  const [dangerConfirm, setDangerConfirm] = useState<{
    keys: string[];
    run: () => Promise<void>;
  } | null>(null);

  // ------------------------------------------------------------------
  // Data fetch
  // ------------------------------------------------------------------

  const fetchData = useCallback(async () => {
    try {
      const [rolesRes, perms, modelsRes, serversRes] = await Promise.all([
        api<{ items: RoleResponse[] }>('/api/admin/roles'),
        api<PermissionDef[]>('/api/admin/permissions'),
        api<{ data: { id: string }[] }>('/v1/models').catch(() => ({ data: [] })),
        api<{ items: McpServer[] }>('/api/mcp/servers').catch(() => ({ items: [] })),
      ]);
      setRoles(rolesRes.items);
      setPermissions(perms);
      setAvailableModels(modelsRes.data.map((m) => m.id));
      setAvailableServers(serversRes.items);
    } catch {
      // silently fail (auth / network); leave previous state
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchData();
  }, [fetchData]);

  const grouped = useMemo(() => groupByResource(permissions), [permissions]);
  const dangerousKeys = useMemo(
    () => new Set(permissions.filter((p) => p.dangerous).map((p) => p.key)),
    [permissions],
  );

  /// Match a single permission key against a search query. The query
  /// is treated as a literal substring unless it contains `*`, in
  /// which case it's compiled to a regex anchored at start/end. This
  /// lets the admin type `*:delete` to find every role with any
  /// `:delete` permission, or `providers:*` to find every role that
  /// touches providers.
  const matchPermission = (perm: string, query: string): boolean => {
    if (!query.includes('*')) return perm.includes(query);
    const escaped = query.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
    try {
      return new RegExp(`^${escaped}$`).test(perm);
    } catch {
      return false;
    }
  };

  /// Collect Action keys out of a policy_document so glob search hits
  /// IAM-mode roles too. Doesn't try to interpret Effect / Resource —
  /// just harvests strings the user might be searching for.
  const extractPolicyActions = (doc: PolicyDocument | null): string[] => {
    if (!doc) return [];
    const out: string[] = [];
    for (const stmt of doc.Statement ?? []) {
      const actions = Array.isArray(stmt.Action) ? stmt.Action : [stmt.Action];
      for (const a of actions) if (typeof a === 'string') out.push(a);
    }
    return out;
  };

  const filteredRoles = useMemo(() => {
    const q = search.trim().toLowerCase();
    return roles.filter((r) => {
      if (filter === 'system' && !r.is_system) return false;
      if (filter === 'custom' && r.is_system) return false;
      if (!q) return true;
      if (r.name.toLowerCase().includes(q)) return true;
      if ((r.description ?? '').toLowerCase().includes(q)) return true;
      const allPerms = [...r.permissions, ...extractPolicyActions(r.policy_document)];
      if (allPerms.some((p) => matchPermission(p.toLowerCase(), q))) return true;
      return false;
    });
  }, [roles, search, filter]);

  // ------------------------------------------------------------------
  // Mutations
  // ------------------------------------------------------------------

  const togglePerm = (set: Set<string>, setFn: (s: Set<string>) => void, perm: string) => {
    const next = new Set(set);
    if (next.has(perm)) next.delete(perm);
    else next.add(perm);
    setFn(next);
  };

  const toggleResourceGroup = (
    set: Set<string>,
    setFn: (s: Set<string>) => void,
    perms: PermissionDef[],
  ) => {
    const next = new Set(set);
    const allOn = perms.every((p) => next.has(p.key));
    if (allOn) {
      for (const p of perms) next.delete(p.key);
    } else {
      for (const p of perms) next.add(p.key);
    }
    setFn(next);
  };

  /// Generic mode-switch handler shared by the create + edit dialogs.
  /// Going simple → policy regenerates the JSON from the perms set so
  /// the policy editor opens on something that already matches what
  /// the admin built. Going policy → simple parses the JSON back into
  /// the perms set, surfacing a warning when the policy contained
  /// constructs (Deny / scoped Resource / conditions) the simple mode
  /// can't represent.
  const switchMode = (
    next: 'simple' | 'policy',
    current: 'simple' | 'policy',
    state: {
      perms: Set<string>;
      setPerms: (p: Set<string>) => void;
      policyJson: string;
      setPolicyJson: (j: string) => void;
      setPolicyError: (e: string) => void;
      setMode: (m: 'simple' | 'policy') => void;
    },
  ) => {
    if (next === current) return;
    if (next === 'policy') {
      state.setPolicyJson(JSON.stringify(permsToPolicy(state.perms), null, 2));
      state.setPolicyError('');
      state.setMode('policy');
      return;
    }
    // policy → simple
    const result = policyToPerms(state.policyJson, permissions);
    if (result.parseError) {
      state.setPolicyError(t('roles.invalidJson'));
      return; // refuse the switch — invalid JSON would silently nuke perms
    }
    state.setPerms(result.perms);
    state.setPolicyError(result.lossy ? t('roles.policySyncLossy') : '');
    state.setMode('simple');
  };

  const resetCreateForm = () => {
    setFormName('');
    setFormDesc('');
    setFormPerms(new Set());
    setFormModels(new Set());
    setFormServers(new Set());
    setFormRestrictModels(false);
    setFormRestrictServers(false);
    setFormPolicyJson('');
    setFormMode('simple');
    setFormPolicyError('');
  };

  /// Compute the set of dangerous permission keys currently selected
  /// in the given perms set. Used to decide whether to gate the
  /// save behind a confirmation dialog.
  const dangerKeysIn = (set: Set<string>): string[] =>
    Array.from(set).filter((k) => dangerousKeys.has(k));

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormPolicyError('');
    let policyDocument: PolicyDocument | null = null;
    if (formMode === 'policy' && formPolicyJson.trim()) {
      try {
        policyDocument = JSON.parse(formPolicyJson) as PolicyDocument;
      } catch {
        setFormPolicyError(t('roles.invalidJson'));
        return;
      }
    }
    const performCreate = async () => {
      setCreating(true);
      try {
        await apiPost('/api/admin/roles', {
          name: formName,
          description: formDesc || null,
          permissions: formMode === 'simple' ? Array.from(formPerms) : [],
          allowed_models: formRestrictModels ? Array.from(formModels) : null,
          allowed_mcp_servers: formRestrictServers ? Array.from(formServers) : null,
          policy_document: policyDocument,
        });
        setCreateOpen(false);
        resetCreateForm();
        fetchData();
      } catch {
        // surfaced via toast elsewhere; keep dialog open
      } finally {
        setCreating(false);
      }
    };

    // Only the simple-mode danger set is checked in the gate. Policy
    // mode is power-user territory and the IAM doc may opt out via
    // explicit Deny rules; we don't try to second-guess it here.
    const danger = formMode === 'simple' ? dangerKeysIn(formPerms) : [];
    if (danger.length > 0) {
      setDangerConfirm({ keys: danger, run: performCreate });
    } else {
      await performCreate();
    }
  };

  const openEdit = (r: RoleResponse) => {
    setEditRole(r);
    setEditName(r.name);
    setEditDesc(r.description || '');
    setEditPerms(new Set(r.permissions));
    setEditRestrictModels(r.allowed_models !== null);
    setEditModels(new Set(r.allowed_models ?? []));
    setEditRestrictServers(r.allowed_mcp_servers !== null);
    setEditServers(new Set(r.allowed_mcp_servers ?? []));
    setEditMode(r.policy_document ? 'policy' : 'simple');
    setEditPolicyJson(r.policy_document ? JSON.stringify(r.policy_document, null, 2) : '');
    setEditPolicyError('');
    setEditOpen(true);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!editRole) return;
    setEditPolicyError('');
    let policyDocument: PolicyDocument | null = null;
    if (editMode === 'policy' && editPolicyJson.trim()) {
      try {
        policyDocument = JSON.parse(editPolicyJson) as PolicyDocument;
      } catch {
        setEditPolicyError(t('roles.invalidJson'));
        return;
      }
    }
    const performEdit = async () => {
      setSaving(true);
      try {
        await apiPatch(`/api/admin/roles/${editRole.id}`, {
          name: editName,
          description: editDesc || null,
          permissions: editMode === 'simple' ? Array.from(editPerms) : [],
          allowed_models: editRestrictModels ? Array.from(editModels) : null,
          allowed_mcp_servers: editRestrictServers ? Array.from(editServers) : null,
          policy_document: policyDocument,
        });
        setEditOpen(false);
        setEditRole(null);
        fetchData();
      } catch {
        // surfaced via toast
      } finally {
        setSaving(false);
      }
    };

    // Only gate when the EDIT introduces dangerous perms that weren't
    // already on the role. Re-saving an existing dangerous role with
    // no new dangerous perms shouldn't pester the admin.
    const oldDanger = new Set((editRole.permissions ?? []).filter((k) => dangerousKeys.has(k)));
    const newDanger =
      editMode === 'simple' ? dangerKeysIn(editPerms).filter((k) => !oldDanger.has(k)) : [];
    if (newDanger.length > 0) {
      setDangerConfirm({ keys: newDanger, run: performEdit });
    } else {
      await performEdit();
    }
  };

  /// Open the delete dialog and lazily fetch the member list so the
  /// per-member migration table has data to show. Resets reassign
  /// state and defaults back to bulk mode.
  const openDelete = async (r: RoleResponse) => {
    setDeleteRole(r);
    setReassignTo('');
    setDeleteError('');
    setDeleteMode('bulk');
    setPerMemberTargets({});
    setDeleteMembers(null);
    if (r.user_count > 0) {
      try {
        const res = await api<{ items: RoleMember[] }>(`/api/admin/roles/${r.id}/members`);
        setDeleteMembers(res.items);
      } catch {
        setDeleteMembers([]);
      }
    }
  };

  const closeDelete = () => {
    setDeleteRole(null);
    setReassignTo('');
    setDeleteError('');
    setDeleteMode('bulk');
    setPerMemberTargets({});
    setDeleteMembers(null);
  };

  const handleDelete = async () => {
    if (!deleteRole) return;
    setDeleting(true);
    setDeleteError('');
    try {
      const needsReassign = deleteRole.user_count > 0;
      if (!needsReassign) {
        await apiDelete(`/api/admin/roles/${deleteRole.id}`);
      } else if (deleteMode === 'bulk') {
        // Atomic single-target migration via the existing query string.
        await apiDelete(`/api/admin/roles/${deleteRole.id}?reassign_to=${reassignTo}`);
      } else {
        // Per-member: PATCH each user first to swap the role, then
        // DELETE the now-empty role without a reassign target. Note
        // this is non-atomic — if a PATCH fails halfway through,
        // some users will already be migrated. The error message
        // surfaces which user the loop bailed on so the admin can
        // recover manually.
        if (!deleteMembers || deleteMembers.length === 0) {
          throw new Error('member list not loaded');
        }
        // Look up every member's current assignments via the user
        // list endpoint. Cheaper than N individual GETs.
        const usersRes = await api<{ data: PickableUser[] }>(
          '/api/admin/users?per_page=1000',
        );
        const usersById = new Map(usersRes.data.map((u) => [u.id, u]));
        for (const m of deleteMembers) {
          const target = perMemberTargets[`${m.user_id}-${m.scope}`];
          if (!target) {
            throw new Error(t('roles.perMemberMissingTarget', { email: m.email }));
          }
          const user = usersById.get(m.user_id);
          if (!user) continue; // user vanished mid-migrate; skip
          const next = user.role_assignments
            // Strip the role being deleted at this exact scope.
            .filter((a) => !(a.role_id === deleteRole.id && a.scope === m.scope))
            // Add the target role at the same scope (skip if already
            // present so we don't duplicate).
            .concat(
              user.role_assignments.some(
                (a) => a.role_id === target && a.scope === m.scope,
              )
                ? []
                : [{ role_id: target, name: '', is_system: false, scope: m.scope }],
            );
          await apiPatch(`/api/admin/users/${user.id}`, {
            role_assignments: next.map((a) => ({ role_id: a.role_id, scope: a.scope })),
          });
        }
        await apiDelete(`/api/admin/roles/${deleteRole.id}`);
      }
      closeDelete();
      fetchData();
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setDeleting(false);
    }
  };

  // ------------------------------------------------------------------
  // Render
  // ------------------------------------------------------------------

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('roles.title')}</h1>
          <p className="text-muted-foreground">{t('roles.subtitle')}</p>
        </div>
        <Dialog
          open={createOpen}
          onOpenChange={(o) => {
            setCreateOpen(o);
            if (!o) resetCreateForm();
          }}
        >
          <DialogTrigger asChild>
            <Button>
              <Plus className="mr-2 h-4 w-4" />
              {t('roles.addRole')}
            </Button>
          </DialogTrigger>
          <DialogContent className="max-w-2xl max-h-[90vh] overflow-y-auto">
            <form onSubmit={handleCreate}>
              <DialogHeader>
                <DialogTitle>{t('roles.addRole')}</DialogTitle>
                <DialogDescription>{t('roles.addRoleDescription')}</DialogDescription>
              </DialogHeader>
              <div className="space-y-4 py-4">
                <div className="grid gap-3 md:grid-cols-2">
                  <div>
                    <Label htmlFor="role-name">{t('common.name')}</Label>
                    <Input
                      id="role-name"
                      value={formName}
                      onChange={(e) => setFormName(e.target.value)}
                      required
                    />
                  </div>
                  <div>
                    <Label htmlFor="role-desc">{t('common.description')}</Label>
                    <Input
                      id="role-desc"
                      value={formDesc}
                      onChange={(e) => setFormDesc(e.target.value)}
                    />
                  </div>
                </div>
                <Tabs
                  value={formMode}
                  onValueChange={(v) =>
                    switchMode(v as 'simple' | 'policy', formMode, {
                      perms: formPerms,
                      setPerms: setFormPerms,
                      policyJson: formPolicyJson,
                      setPolicyJson: setFormPolicyJson,
                      setPolicyError: setFormPolicyError,
                      setMode: setFormMode,
                    })
                  }
                >
                  <TabsList className="grid w-full grid-cols-2">
                    <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                    <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                  </TabsList>
                  <TabsContent value="simple" className="space-y-4 mt-3">
                    {/* Starter pickers — admins can either clone an
                        existing role or drop in one of the curated
                        from-scratch templates. Both reset the rest of
                        the form fields. */}
                    <div className="grid gap-3 md:grid-cols-2">
                      <div>
                        <Label className="text-sm font-medium">{t('roles.cloneFrom')}</Label>
                        <p className="text-xs text-muted-foreground mb-1.5">
                          {t('roles.cloneFromDesc')}
                        </p>
                        <Select
                          value=""
                          onValueChange={(roleId) => {
                            const src = roles.find((r) => r.id === roleId);
                            if (!src) return;
                            setFormPerms(new Set(src.permissions));
                            setFormRestrictModels(src.allowed_models !== null);
                            setFormModels(new Set(src.allowed_models ?? []));
                            setFormRestrictServers(src.allowed_mcp_servers !== null);
                            setFormServers(new Set(src.allowed_mcp_servers ?? []));
                          }}
                        >
                          <SelectTrigger>
                            <SelectValue placeholder={t('roles.cloneFromPlaceholder')} />
                          </SelectTrigger>
                          <SelectContent>
                            {roles.map((r) => (
                              <SelectItem key={r.id} value={r.id}>
                                <span className="font-mono text-xs">{r.name}</span>
                                {r.is_system && (
                                  <span className="ml-2 text-[10px] text-muted-foreground">
                                    {t('roles.systemRole')}
                                  </span>
                                )}
                              </SelectItem>
                            ))}
                          </SelectContent>
                        </Select>
                      </div>
                      <div>
                        <Label className="text-sm font-medium">{t('roles.startFromTemplate')}</Label>
                        <p className="text-xs text-muted-foreground mb-1.5">
                          {t('roles.startFromTemplateDesc')}
                        </p>
                        <Select
                          value=""
                          onValueChange={(tplId) => {
                            const tpl = SIMPLE_TEMPLATES.find((x) => x.id === tplId);
                            if (!tpl) return;
                            // Drop any keys not in the live catalog so a
                            // stale template never injects unknown perms.
                            const valid = new Set(permissions.map((p) => p.key));
                            setFormPerms(new Set(tpl.permissions.filter((k) => valid.has(k))));
                            setFormRestrictModels(false);
                            setFormModels(new Set());
                            setFormRestrictServers(false);
                            setFormServers(new Set());
                          }}
                        >
                          <SelectTrigger>
                            <SelectValue placeholder={t('roles.pickTemplate')} />
                          </SelectTrigger>
                          <SelectContent>
                            {SIMPLE_TEMPLATES.map((tpl) => (
                              <SelectItem key={tpl.id} value={tpl.id}>
                                {t(`roles.template_${tpl.id}` as const, {
                                  defaultValue: tpl.id,
                                })}
                              </SelectItem>
                            ))}
                          </SelectContent>
                        </Select>
                      </div>
                    </div>
                    <PermissionTree
                      grouped={grouped}
                      selected={formPerms}
                      onTogglePerm={(p) => togglePerm(formPerms, setFormPerms, p)}
                      onToggleGroup={(perms) => toggleResourceGroup(formPerms, setFormPerms, perms)}
                      onSelectAll={() => setFormPerms(new Set(permissions.map((p) => p.key)))}
                      onClear={() => setFormPerms(new Set())}
                      renderExtras={(resource) => {
                        if (resource === 'ai_gateway' && formPerms.has('ai_gateway:use')) {
                          return (
                            <ModelConstraint
                              restrict={formRestrictModels}
                              onRestrictChange={setFormRestrictModels}
                              selected={formModels}
                              onToggle={(id) => togglePerm(formModels, setFormModels, id)}
                              available={availableModels}
                            />
                          );
                        }
                        if (resource === 'mcp_gateway' && formPerms.has('mcp_gateway:use')) {
                          return (
                            <ServerConstraint
                              restrict={formRestrictServers}
                              onRestrictChange={setFormRestrictServers}
                              selected={formServers}
                              onToggle={(id) => togglePerm(formServers, setFormServers, id)}
                              available={availableServers}
                            />
                          );
                        }
                        return null;
                      }}
                    />
                  </TabsContent>
                  <TabsContent value="policy" className="mt-3">
                    <PolicyEditor
                      value={formPolicyJson}
                      onChange={setFormPolicyJson}
                      error={formPolicyError}
                      onApplyTemplate={(tpl) => setFormPolicyJson(JSON.stringify(tpl, null, 2))}
                    />
                  </TabsContent>
                </Tabs>

                {formMode === 'simple' && hasDangerous(formPerms, dangerousKeys) && (
                  <DangerPermissionWarning />
                )}
              </div>
              <DialogFooter>
                <Button variant="outline" type="button" onClick={() => setCreateOpen(false)}>
                  {t('common.cancel')}
                </Button>
                <Button type="submit" disabled={creating || !formName}>
                  {creating ? t('common.loading') : t('common.create')}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {/* Filters */}
      <div className="flex items-center gap-2">
        <div className="relative flex-1 max-w-sm">
          <Search className="absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('roles.searchPlaceholder')}
            className="pl-8"
          />
        </div>
        <RoleKindTabs value={filter} onChange={setFilter} counts={countsByKind(roles)} />
      </div>

      {/* Unified table */}
      <Card className="gap-0 py-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-[200px]">{t('roles.colRole')}</TableHead>
              <TableHead>{t('roles.colDescription')}</TableHead>
              <TableHead className="w-[140px]">{t('roles.colPermissions')}</TableHead>
              <TableHead className="w-[80px] text-right">{t('roles.colUsers')}</TableHead>
              <TableHead className="w-[60px] text-right">{t('common.actions')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {loading ? (
              [...Array(5)].map((_, i) => (
                <TableRow key={i}>
                  <TableCell colSpan={5}>
                    <Skeleton className="h-5 w-full" />
                  </TableCell>
                </TableRow>
              ))
            ) : filteredRoles.length === 0 ? (
              <TableRow>
                <TableCell colSpan={5} className="py-12 text-center text-muted-foreground">
                  <Shield className="mx-auto mb-2 h-8 w-8 text-muted-foreground/30" />
                  <p className="text-sm">{t('roles.noRoles')}</p>
                </TableCell>
              </TableRow>
            ) : (
              filteredRoles.map((role) => (
                <TableRow
                  key={role.id}
                  className="cursor-pointer hover:bg-muted/30"
                  onClick={() => setDetailRole(role)}
                >
                  <TableCell>
                    <div className="flex items-center gap-2">
                      {role.is_system ? (
                        <Lock className="h-3.5 w-3.5 text-muted-foreground" />
                      ) : (
                        <Shield className="h-3.5 w-3.5 text-muted-foreground" />
                      )}
                      <span className="font-mono text-sm">{role.name}</span>
                      {role.is_system && (
                        <Badge variant="secondary" className="text-[10px]">
                          {t('roles.systemRole')}
                        </Badge>
                      )}
                    </div>
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    <span className="line-clamp-1">{role.description || '—'}</span>
                  </TableCell>
                  <TableCell>
                    {role.policy_document ? (
                      <PolicyPermSummary
                        doc={role.policy_document}
                        catalog={permissions}
                      />
                    ) : (
                      <span className="font-mono text-xs tabular-nums">
                        {role.permissions.length}
                      </span>
                    )}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {role.user_count}
                  </TableCell>
                  <TableCell
                    className="text-right"
                    onClick={(e) => e.stopPropagation()}
                  >
                    <div className="flex justify-end gap-1">
                      {!role.is_system && (
                        <>
                          <Button
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7"
                            onClick={() => openEdit(role)}
                            aria-label={t('common.edit')}
                          >
                            <Pencil className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7 text-destructive"
                            onClick={() => openDelete(role)}
                            aria-label={t('common.delete')}
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                          </Button>
                        </>
                      )}
                    </div>
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </Card>

      {/* Detail drawer */}
      <Dialog open={!!detailRole} onOpenChange={(o) => !o && setDetailRole(null)}>
        <DialogContent className="max-w-2xl max-h-[90vh] overflow-y-auto">
          {detailRole && (
            <RoleDetail
              role={detailRole}
              grouped={grouped}
              dangerousKeys={dangerousKeys}
              availableServers={availableServers}
              onMembersChanged={fetchData}
            />
          )}
        </DialogContent>
      </Dialog>

      {/* Edit dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-w-2xl max-h-[90vh] overflow-y-auto">
          <form onSubmit={handleEdit}>
            <DialogHeader>
              <DialogTitle>{t('roles.editRole')}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="grid gap-3 md:grid-cols-2">
                <div>
                  <Label htmlFor="edit-name">{t('common.name')}</Label>
                  <Input
                    id="edit-name"
                    value={editName}
                    onChange={(e) => setEditName(e.target.value)}
                    required
                  />
                </div>
                <div>
                  <Label htmlFor="edit-desc">{t('common.description')}</Label>
                  <Input
                    id="edit-desc"
                    value={editDesc}
                    onChange={(e) => setEditDesc(e.target.value)}
                  />
                </div>
              </div>
              <Tabs
                value={editMode}
                onValueChange={(v) =>
                  switchMode(v as 'simple' | 'policy', editMode, {
                    perms: editPerms,
                    setPerms: setEditPerms,
                    policyJson: editPolicyJson,
                    setPolicyJson: setEditPolicyJson,
                    setPolicyError: setEditPolicyError,
                    setMode: setEditMode,
                  })
                }
              >
                <TabsList className="grid w-full grid-cols-2">
                  <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                  <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                </TabsList>
                <TabsContent value="simple" className="space-y-4 mt-3">
                  <PermissionTree
                    grouped={grouped}
                    selected={editPerms}
                    onTogglePerm={(p) => togglePerm(editPerms, setEditPerms, p)}
                    onToggleGroup={(perms) => toggleResourceGroup(editPerms, setEditPerms, perms)}
                    onSelectAll={() => setEditPerms(new Set(permissions.map((p) => p.key)))}
                    onClear={() => setEditPerms(new Set())}
                    renderExtras={(resource) => {
                      if (resource === 'ai_gateway' && editPerms.has('ai_gateway:use')) {
                        return (
                          <ModelConstraint
                            restrict={editRestrictModels}
                            onRestrictChange={setEditRestrictModels}
                            selected={editModels}
                            onToggle={(id) => togglePerm(editModels, setEditModels, id)}
                            available={availableModels}
                          />
                        );
                      }
                      if (resource === 'mcp_gateway' && editPerms.has('mcp_gateway:use')) {
                        return (
                          <ServerConstraint
                            restrict={editRestrictServers}
                            onRestrictChange={setEditRestrictServers}
                            selected={editServers}
                            onToggle={(id) => togglePerm(editServers, setEditServers, id)}
                            available={availableServers}
                          />
                        );
                      }
                      return null;
                    }}
                  />
                </TabsContent>
                <TabsContent value="policy" className="mt-3">
                  <PolicyEditor
                    value={editPolicyJson}
                    onChange={setEditPolicyJson}
                    error={editPolicyError}
                    onApplyTemplate={(tpl) => setEditPolicyJson(JSON.stringify(tpl, null, 2))}
                  />
                </TabsContent>
              </Tabs>
              {editMode === 'simple' && hasDangerous(editPerms, dangerousKeys) && (
                <DangerPermissionWarning />
              )}
            </div>
            <DialogFooter>
              <Button variant="outline" type="button" onClick={() => setEditOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={saving || !editName}>
                {saving ? t('common.loading') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Delete with optional reassign */}
      <Dialog
        open={!!deleteRole}
        onOpenChange={(o) => {
          if (!o) closeDelete();
        }}
      >
        <DialogContent className="max-w-2xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('roles.deleteRole')}</DialogTitle>
            <DialogDescription>
              {deleteRole?.user_count
                ? t('roles.deleteWithMembers', {
                    name: deleteRole?.name,
                    count: deleteRole?.user_count,
                  })
                : t('roles.deleteConfirm')}
            </DialogDescription>
          </DialogHeader>
          {deleteRole && deleteRole.user_count > 0 && (
            <div className="space-y-3 py-2">
              <Tabs
                value={deleteMode}
                onValueChange={(v) => setDeleteMode(v as 'bulk' | 'per_member')}
              >
                <TabsList className="grid w-full grid-cols-2">
                  <TabsTrigger value="bulk">{t('roles.deleteBulk')}</TabsTrigger>
                  <TabsTrigger value="per_member">{t('roles.deletePerMember')}</TabsTrigger>
                </TabsList>
                <TabsContent value="bulk" className="space-y-2 mt-3">
                  <Label>{t('roles.reassignTo')}</Label>
                  <Select value={reassignTo} onValueChange={setReassignTo}>
                    <SelectTrigger>
                      <SelectValue placeholder={t('roles.reassignToPlaceholder')} />
                    </SelectTrigger>
                    <SelectContent>
                      {roles
                        .filter((r) => r.id !== deleteRole.id)
                        .map((r) => (
                          <SelectItem key={r.id} value={r.id}>
                            <span className="font-mono text-xs">{r.name}</span>
                            {r.is_system && (
                              <span className="ml-2 text-[10px] text-muted-foreground">
                                {t('roles.systemRole')}
                              </span>
                            )}
                          </SelectItem>
                        ))}
                    </SelectContent>
                  </Select>
                </TabsContent>
                <TabsContent value="per_member" className="mt-3">
                  <p className="mb-2 text-xs text-muted-foreground">
                    {t('roles.deletePerMemberDesc')}
                  </p>
                  {deleteMembers === null ? (
                    <p className="text-xs text-muted-foreground italic">
                      {t('common.loading')}
                    </p>
                  ) : (
                    <ScrollArea className="max-h-72 rounded-md border">
                      <div className="divide-y">
                        {deleteMembers.map((m) => {
                          const key = `${m.user_id}-${m.scope}`;
                          return (
                            <div
                              key={key}
                              className="flex items-center gap-2 px-3 py-2 text-xs"
                            >
                              <span className="min-w-0 flex-1 truncate font-mono">
                                {m.email}
                              </span>
                              {m.scope !== 'global' && (
                                <Badge variant="outline" className="text-[9px]">
                                  {m.scope}
                                </Badge>
                              )}
                              <Select
                                value={perMemberTargets[key] ?? ''}
                                onValueChange={(v) =>
                                  setPerMemberTargets({
                                    ...perMemberTargets,
                                    [key]: v,
                                  })
                                }
                              >
                                <SelectTrigger className="h-7 w-44">
                                  <SelectValue
                                    placeholder={t('roles.reassignToPlaceholder')}
                                  />
                                </SelectTrigger>
                                <SelectContent>
                                  {roles
                                    .filter((r) => r.id !== deleteRole.id)
                                    .map((r) => (
                                      <SelectItem key={r.id} value={r.id}>
                                        <span className="font-mono text-xs">{r.name}</span>
                                      </SelectItem>
                                    ))}
                                </SelectContent>
                              </Select>
                            </div>
                          );
                        })}
                      </div>
                    </ScrollArea>
                  )}
                </TabsContent>
              </Tabs>
            </div>
          )}
          {deleteError && (
            <div className="rounded-md border border-destructive/30 bg-destructive/10 p-2 text-xs text-destructive">
              {deleteError}
            </div>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={closeDelete}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="destructive"
              disabled={
                deleting ||
                (!!deleteRole?.user_count &&
                  ((deleteMode === 'bulk' && !reassignTo) ||
                    (deleteMode === 'per_member' &&
                      (!deleteMembers ||
                        deleteMembers.some(
                          (m) => !perMemberTargets[`${m.user_id}-${m.scope}`],
                        )))))
              }
              onClick={handleDelete}
            >
              {deleting ? t('common.loading') : t('common.delete')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Dangerous-permission save confirmation. Surfaces the exact
          set of dangerous keys the admin is about to grant so they
          can't blindly tab through. */}
      <Dialog
        open={!!dangerConfirm}
        onOpenChange={(o) => {
          if (!o) setDangerConfirm(null);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2 text-destructive">
              <AlertTriangle className="h-4 w-4" />
              {t('roles.dangerConfirmTitle')}
            </DialogTitle>
            <DialogDescription>{t('roles.dangerConfirmDesc')}</DialogDescription>
          </DialogHeader>
          {dangerConfirm && (
            <div className="rounded-md border border-destructive/30 bg-destructive/10 p-3">
              <ul className="space-y-1 font-mono text-xs text-destructive">
                {dangerConfirm.keys.map((k) => (
                  <li key={k}>• {k}</li>
                ))}
              </ul>
            </div>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setDangerConfirm(null)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="destructive"
              onClick={async () => {
                const action = dangerConfirm;
                setDangerConfirm(null);
                if (action) await action.run();
              }}
            >
              {t('roles.dangerConfirmAccept')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Sub-components
// ----------------------------------------------------------------------------

function countsByKind(roles: RoleResponse[]) {
  return {
    all: roles.length,
    system: roles.filter((r) => r.is_system).length,
    custom: roles.filter((r) => !r.is_system).length,
  };
}

function RoleKindTabs({
  value,
  onChange,
  counts,
}: {
  value: 'all' | 'system' | 'custom';
  onChange: (v: 'all' | 'system' | 'custom') => void;
  counts: { all: number; system: number; custom: number };
}) {
  const { t } = useTranslation();
  const tabs: { key: 'all' | 'system' | 'custom'; label: string }[] = [
    { key: 'all', label: t('common.all') },
    { key: 'system', label: t('roles.systemRoles') },
    { key: 'custom', label: t('roles.customRoles') },
  ];
  return (
    <div
      role="tablist"
      aria-label={t('roles.filterByKind')}
      className="flex items-center gap-px rounded-md border bg-muted/40 p-px text-xs"
    >
      {tabs.map((tab) => {
        const active = value === tab.key;
        return (
          <button
            key={tab.key}
            type="button"
            role="tab"
            aria-selected={active}
            tabIndex={active ? 0 : -1}
            onClick={() => onChange(tab.key)}
            className={`rounded px-2 py-1 transition-colors ${
              active
                ? 'bg-background text-foreground'
                : 'text-muted-foreground hover:text-foreground'
            }`}
          >
            {tab.label}
            <span className="ml-1.5 tabular-nums opacity-60">{counts[tab.key]}</span>
          </button>
        );
      })}
    </div>
  );
}

function hasDangerous(selected: Set<string>, dangerous: Set<string>): boolean {
  for (const key of selected) if (dangerous.has(key)) return true;
  return false;
}

function DangerPermissionWarning() {
  const { t } = useTranslation();
  return (
    <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 p-2 text-xs text-destructive">
      <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
      <div>{t('roles.dangerWarning')}</div>
    </div>
  );
}

/// Tree-style permission picker grouped by resource. Each resource section
/// has a parent checkbox that toggles all of its actions; dangerous actions
/// are highlighted so the admin notices what they're granting. The header
/// row exposes Select All / Clear shortcuts. `renderExtras` lets the caller
/// inject resource-scoped constraint UI (e.g. allowed-models picker for
/// `ai_gateway`) directly under the matching permission group.
function PermissionTree({
  grouped,
  selected,
  onTogglePerm,
  onToggleGroup,
  onSelectAll,
  onClear,
  renderExtras,
}: {
  grouped: Map<string, PermissionDef[]>;
  selected: Set<string>;
  onTogglePerm: (key: string) => void;
  onToggleGroup: (perms: PermissionDef[]) => void;
  onSelectAll: () => void;
  onClear: () => void;
  renderExtras?: (resource: string) => ReactNode;
}) {
  const { t } = useTranslation();
  const groups = Array.from(grouped.entries());
  return (
    <div>
      <div className="flex items-center justify-between">
        <Label className="text-sm font-medium">{t('roles.permissions')}</Label>
        <div className="flex items-center gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-xs"
            onClick={onSelectAll}
          >
            {t('common.selectAll')}
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-xs"
            onClick={onClear}
          >
            {t('common.clearAll')}
          </Button>
        </div>
      </div>
      <ScrollArea className="mt-2 h-72 rounded-md border">
        <div className="divide-y">
          {groups.map(([resource, perms]) => {
            const allOn = perms.every((p) => selected.has(p.key));
            const someOn = !allOn && perms.some((p) => selected.has(p.key));
            return (
              <div key={resource} className="px-3 py-2">
                <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
                  <Checkbox
                    checked={allOn}
                    data-state={someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'}
                    onCheckedChange={() => onToggleGroup(perms)}
                  />
                  <span className="font-mono uppercase tracking-wider text-muted-foreground">
                    {t(`permissions.resource.${resource}` as const, {
                      defaultValue: resource,
                    })}
                  </span>
                </label>
                <div className="mt-1.5 grid grid-cols-2 gap-x-4 gap-y-1 pl-6">
                  {perms.map((p) => (
                    <label
                      key={p.key}
                      className="flex cursor-pointer items-center gap-1.5 text-xs"
                      title={p.key}
                    >
                      <Checkbox
                        checked={selected.has(p.key)}
                        onCheckedChange={() => onTogglePerm(p.key)}
                      />
                      <span className={p.dangerous ? 'text-destructive' : ''}>
                        {t(`permissions.action.${p.action}` as const, {
                          defaultValue: p.action,
                        })}
                      </span>
                      {p.dangerous && (
                        <AlertTriangle
                          className="h-3 w-3 shrink-0 text-destructive"
                          aria-label={t('roles.dangerous')}
                        />
                      )}
                    </label>
                  ))}
                </div>
                {renderExtras?.(resource)}
              </div>
            );
          })}
        </div>
      </ScrollArea>
    </div>
  );
}

/// Inline allowed-models picker rendered under the `ai_gateway` permission
/// group. Collapsed by default — admin must opt in to "limit to specific
/// models", which then exposes the per-model checkbox grid.
function ModelConstraint({
  restrict,
  onRestrictChange,
  selected,
  onToggle,
  available,
}: {
  restrict: boolean;
  onRestrictChange: (v: boolean) => void;
  selected: Set<string>;
  onToggle: (id: string) => void;
  available: string[];
}) {
  const { t } = useTranslation();
  return (
    <div className="mt-2 ml-6 rounded border bg-muted/20 p-2 space-y-1.5">
      <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
        <Checkbox checked={restrict} onCheckedChange={(v) => onRestrictChange(!!v)} />
        <span>{t('roles.allowedModels')}</span>
        <span className="font-normal text-muted-foreground">
          — {restrict ? `${selected.size}` : t('roles.allModels')}
        </span>
      </label>
      {restrict &&
        (available.length > 0 ? (
          <ScrollArea className="max-h-32">
            <div className="grid grid-cols-2 gap-1 pl-5">
              {available.map((model) => (
                <label
                  key={model}
                  className="flex cursor-pointer items-center gap-1.5 text-xs"
                >
                  <Checkbox
                    checked={selected.has(model)}
                    onCheckedChange={() => onToggle(model)}
                  />
                  <span className="truncate">{model}</span>
                </label>
              ))}
            </div>
          </ScrollArea>
        ) : (
          <p className="pl-5 text-xs italic text-muted-foreground">{t('common.noData')}</p>
        ))}
    </div>
  );
}

/// Inline allowed-MCP-servers picker rendered under the `mcp_gateway`
/// permission group. Same opt-in model as `ModelConstraint`.
function ServerConstraint({
  restrict,
  onRestrictChange,
  selected,
  onToggle,
  available,
}: {
  restrict: boolean;
  onRestrictChange: (v: boolean) => void;
  selected: Set<string>;
  onToggle: (id: string) => void;
  available: McpServer[];
}) {
  const { t } = useTranslation();
  return (
    <div className="mt-2 ml-6 rounded border bg-muted/20 p-2 space-y-1.5">
      <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
        <Checkbox checked={restrict} onCheckedChange={(v) => onRestrictChange(!!v)} />
        <span>{t('roles.allowedMcpServers')}</span>
        <span className="font-normal text-muted-foreground">
          — {restrict ? `${selected.size}` : t('roles.allServers')}
        </span>
      </label>
      {restrict &&
        (available.length > 0 ? (
          <ScrollArea className="max-h-32">
            <div className="grid grid-cols-1 gap-1 pl-5">
              {available.map((srv) => (
                <label
                  key={srv.id}
                  className="flex cursor-pointer items-center gap-1.5 text-xs"
                >
                  <Checkbox
                    checked={selected.has(srv.id)}
                    onCheckedChange={() => onToggle(srv.id)}
                  />
                  <span className="truncate">{srv.name}</span>
                </label>
              ))}
            </div>
          </ScrollArea>
        ) : (
          <p className="pl-5 text-xs italic text-muted-foreground">{t('common.noData')}</p>
        ))}
    </div>
  );
}

/// One row from the admin user list, used by RoleDetail's "add
/// member" picker. Only the fields the picker needs to display and
/// to PATCH a role assignment back are pulled in here.
interface PickableUser {
  id: string;
  email: string;
  display_name: string;
  role_assignments: Array<{
    role_id: string;
    name: string;
    is_system: boolean;
    scope: string;
  }>;
}

function RoleDetail({
  role,
  grouped,
  dangerousKeys,
  availableServers,
  onMembersChanged,
}: {
  role: RoleResponse;
  grouped: Map<string, PermissionDef[]>;
  dangerousKeys: Set<string>;
  availableServers: McpServer[];
  onMembersChanged: () => void;
}) {
  const { t } = useTranslation();
  const selected = new Set(role.permissions);

  // Fetch members lazily on open. The list lives outside the cached
  // /api/admin/roles snapshot so it can be slow without slowing the
  // initial table render.
  const [members, setMembers] = useState<RoleMember[] | null>(null);
  const [membersError, setMembersError] = useState(false);
  const reloadMembers = useCallback(async () => {
    try {
      const res = await api<{ items: RoleMember[] }>(`/api/admin/roles/${role.id}/members`);
      setMembers(res.items);
      setMembersError(false);
    } catch {
      setMembersError(true);
    }
  }, [role.id]);
  useEffect(() => {
    setMembers(null);
    setMembersError(false);
    reloadMembers();
  }, [reloadMembers]);

  // Lazy user picker for the "add member" form. Fetched on demand
  // (clicking the add button) so opening the dialog doesn't always
  // pay the cost — most viewings are read-only.
  const [users, setUsers] = useState<PickableUser[] | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [picking, setPicking] = useState('');
  const [busy, setBusy] = useState(false);
  const [memberError, setMemberError] = useState('');
  const ensureUsers = async () => {
    if (users !== null) return;
    try {
      const res = await api<{ data: PickableUser[] }>('/api/admin/users');
      setUsers(res.data);
    } catch (e) {
      setMemberError(e instanceof Error ? e.message : 'Failed to load users');
    }
  };

  /// Update one user's role_assignments via PATCH /api/admin/users/{id}.
  /// Used by both the add and remove flows. The backend's update_user
  /// applies replace-all semantics, so we have to send the full
  /// assignment list, not a diff.
  const writeAssignments = async (
    user: PickableUser,
    next: PickableUser['role_assignments'],
  ) => {
    await apiPatch(`/api/admin/users/${user.id}`, {
      role_assignments: next.map((a) => ({ role_id: a.role_id, scope: a.scope })),
    });
  };

  const addMember = async () => {
    if (!picking || !users) return;
    const user = users.find((u) => u.id === picking);
    if (!user) return;
    if (user.role_assignments.some((a) => a.role_id === role.id && a.scope === 'global')) {
      setMemberError(t('roles.memberAlreadyAssigned'));
      return;
    }
    setBusy(true);
    setMemberError('');
    try {
      const next = [
        ...user.role_assignments,
        { role_id: role.id, name: role.name, is_system: role.is_system, scope: 'global' },
      ];
      await writeAssignments(user, next);
      // Locally patch the cached user list so subsequent picks reflect
      // the change without a refetch.
      setUsers(
        (users ?? []).map((u) => (u.id === user.id ? { ...u, role_assignments: next } : u)),
      );
      setPicking('');
      setPickerOpen(false);
      await reloadMembers();
      onMembersChanged();
    } catch (e) {
      setMemberError(e instanceof Error ? e.message : 'Failed');
    } finally {
      setBusy(false);
    }
  };

  const removeMember = async (m: RoleMember) => {
    setBusy(true);
    setMemberError('');
    try {
      // Read-modify-write the user. We could be operating on stale
      // local state but the backend's PATCH is replace-all, so we
      // refetch this user's current assignments first.
      const fresh = await api<{ data: PickableUser[] }>(
        `/api/admin/users?per_page=1000`,
      );
      const user = fresh.data.find((u) => u.id === m.user_id);
      if (!user) {
        setMemberError(t('roles.memberNotFound'));
        return;
      }
      const next = user.role_assignments.filter(
        (a) => !(a.role_id === role.id && a.scope === m.scope),
      );
      await writeAssignments(user, next);
      setUsers(fresh.data);
      await reloadMembers();
      onMembersChanged();
    } catch (e) {
      setMemberError(e instanceof Error ? e.message : 'Failed');
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex items-center gap-2">
          {role.is_system ? (
            <Lock className="h-4 w-4 text-muted-foreground" />
          ) : (
            <Shield className="h-4 w-4 text-muted-foreground" />
          )}
          <span className="font-mono">{role.name}</span>
          {role.is_system && (
            <Badge variant="secondary" className="text-[10px]">
              {t('roles.systemRole')}
            </Badge>
          )}
        </DialogTitle>
        <DialogDescription>{role.description || '—'}</DialogDescription>
      </DialogHeader>
      <div className="space-y-4 py-2">
        <div>
          <div className="mb-2 text-xs uppercase tracking-wider text-muted-foreground">
            {t('roles.colUsers')}
          </div>
          <div className="font-mono text-2xl tabular-nums">{role.user_count}</div>
        </div>
        {role.policy_document ? (
          <div>
            <div className="mb-2 flex items-center gap-1.5 text-xs uppercase tracking-wider text-muted-foreground">
              <FileJson className="h-3.5 w-3.5" />
              {t('roles.policyDocument')}
            </div>
            <pre className="max-h-72 overflow-auto rounded-md border bg-muted/30 p-3 font-mono text-[10px]">
              {JSON.stringify(role.policy_document, null, 2)}
            </pre>
          </div>
        ) : (
          <div>
            <div className="mb-2 text-xs uppercase tracking-wider text-muted-foreground">
              {t('roles.permissions')}
            </div>
            <div className="space-y-2 rounded-md border p-3">
              {Array.from(grouped.entries()).map(([resource, perms]) => {
                const granted = perms.filter((p) => selected.has(p.key));
                if (granted.length === 0) return null;
                return (
                  <div key={resource}>
                    <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                      {t(`permissions.resource.${resource}` as const, {
                        defaultValue: resource,
                      })}
                    </div>
                    <div className="mt-1 flex flex-wrap gap-1">
                      {granted.map((p) => (
                        <Badge
                          key={p.key}
                          variant={dangerousKeys.has(p.key) ? 'destructive' : 'outline'}
                          className="text-[10px]"
                        >
                          {t(`permissions.action.${p.action}` as const, {
                            defaultValue: p.action,
                          })}
                        </Badge>
                      ))}
                    </div>
                  </div>
                );
              })}
              {role.permissions.length === 0 && (
                <span className="text-xs text-muted-foreground italic">
                  {t('common.none')}
                </span>
              )}
            </div>
          </div>
        )}
        {(role.allowed_models !== null || role.allowed_mcp_servers !== null) && (
          <div className="space-y-2">
            {role.allowed_models !== null && (
              <ConstraintRow
                label={t('roles.allowedModels')}
                items={role.allowed_models}
                resolveLabel={(s) => s}
              />
            )}
            {role.allowed_mcp_servers !== null && (
              <ConstraintRow
                label={t('roles.allowedMcpServers')}
                items={role.allowed_mcp_servers}
                resolveLabel={(id) =>
                  availableServers.find((s) => s.id === id)?.name ?? id.slice(0, 8)
                }
              />
            )}
          </div>
        )}
        {/* Members — who's actually using this role today, with
            inline add / remove. */}
        <div>
          <div className="mb-2 flex items-center justify-between">
            <span className="text-xs uppercase tracking-wider text-muted-foreground">
              {t('roles.members')}
            </span>
            {!pickerOpen && (
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="h-6 px-2 text-xs"
                onClick={async () => {
                  setMemberError('');
                  setPickerOpen(true);
                  await ensureUsers();
                }}
              >
                <Plus className="mr-1 h-3 w-3" />
                {t('roles.addMember')}
              </Button>
            )}
          </div>
          {pickerOpen && (
            <div className="mb-2 flex items-center gap-2 rounded-md border bg-muted/20 p-2">
              <div className="flex-1">
                <Select value={picking} onValueChange={setPicking}>
                  <SelectTrigger className="h-8">
                    <SelectValue placeholder={t('roles.pickUser')} />
                  </SelectTrigger>
                  <SelectContent>
                    {users === null ? (
                      <div className="px-2 py-1.5 text-xs text-muted-foreground">
                        {t('common.loading')}
                      </div>
                    ) : (
                      users
                        // Hide users who already hold this role at global scope.
                        .filter(
                          (u) =>
                            !u.role_assignments.some(
                              (a) => a.role_id === role.id && a.scope === 'global',
                            ),
                        )
                        .map((u) => (
                          <SelectItem key={u.id} value={u.id}>
                            <span className="font-mono text-xs">{u.email}</span>
                            {u.display_name && (
                              <span className="ml-2 text-[10px] text-muted-foreground">
                                {u.display_name}
                              </span>
                            )}
                          </SelectItem>
                        ))
                    )}
                  </SelectContent>
                </Select>
              </div>
              <Button
                type="button"
                size="sm"
                className="h-8"
                disabled={!picking || busy}
                onClick={addMember}
              >
                {busy ? t('common.loading') : t('common.add')}
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-8"
                onClick={() => {
                  setPickerOpen(false);
                  setPicking('');
                  setMemberError('');
                }}
              >
                {t('common.cancel')}
              </Button>
            </div>
          )}
          {memberError && (
            <p className="mb-1 text-[11px] text-destructive">{memberError}</p>
          )}
          {members === null ? (
            <div className="text-xs text-muted-foreground italic">
              {membersError ? t('common.error') : t('common.loading')}
            </div>
          ) : members.length === 0 ? (
            <div className="text-xs italic text-muted-foreground">{t('roles.noMembers')}</div>
          ) : (
            <ScrollArea className="max-h-48 rounded-md border">
              <div className="divide-y">
                {members.map((m) => (
                  <div
                    key={`${m.user_id}-${m.source}-${m.scope}`}
                    className="flex items-center gap-2 px-3 py-1.5 text-xs"
                  >
                    <span className="min-w-0 flex-1 truncate font-mono">{m.email}</span>
                    {m.display_name && (
                      <span className="hidden truncate text-muted-foreground sm:inline">
                        {m.display_name}
                      </span>
                    )}
                    {m.scope !== 'global' && (
                      <Badge variant="outline" className="text-[9px]">
                        {m.scope}
                      </Badge>
                    )}
                    <Badge
                      variant={m.source === 'system' ? 'secondary' : 'outline'}
                      className="text-[9px]"
                    >
                      {m.source === 'system' ? t('roles.systemRole') : t('roles.customRoles')}
                    </Badge>
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6 text-destructive"
                      disabled={busy}
                      onClick={() => removeMember(m)}
                      aria-label={t('common.delete')}
                    >
                      <Trash2 className="h-3 w-3" />
                    </Button>
                  </div>
                ))}
              </div>
            </ScrollArea>
          )}
        </div>
      </div>
    </>
  );
}

/// Estimate the effective permission count for a policy_document by
/// expanding wildcard Allow actions against the static catalog and
/// subtracting Deny matches. Used in the role list to show a single
/// number for IAM-mode roles instead of just "policy", and to render
/// a hover preview of the first few statements.
///
/// This is a static approximation: it ignores Conditions, treats
/// Resource as either `*` (full) or scoped (skipped from the count),
/// and assumes the Action grammar matches `prefix:*` / `*:suffix`
/// glob style. Power-user policies that depend on conditional logic
/// will be undercounted; the docstring on the row tooltip says so.
function PolicyPermSummary({
  doc,
  catalog,
}: {
  doc: PolicyDocument;
  catalog: PermissionDef[];
}) {
  const { t } = useTranslation();

  const matchAction = (pattern: string, key: string): boolean => {
    if (pattern === '*') return true;
    if (!pattern.includes('*')) return pattern === key;
    const escaped = pattern.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
    try {
      return new RegExp(`^${escaped}$`).test(key);
    } catch {
      return false;
    }
  };
  const isWildcardResource = (r: PolicyStatement['Resource']): boolean => {
    if (r === '*') return true;
    if (Array.isArray(r)) return r.includes('*');
    return false;
  };

  const granted = new Set<string>();
  for (const stmt of doc.Statement ?? []) {
    if (!isWildcardResource(stmt.Resource)) continue;
    const actions = Array.isArray(stmt.Action) ? stmt.Action : [stmt.Action];
    for (const p of catalog) {
      if (actions.some((a) => matchAction(a, p.key))) {
        if (stmt.Effect === 'Allow') granted.add(p.key);
        else if (stmt.Effect === 'Deny') granted.delete(p.key);
      }
    }
  }

  // Tooltip: first three statements compactly summarized.
  const summarize = (stmt: PolicyStatement): string => {
    const acts = Array.isArray(stmt.Action) ? stmt.Action.join(', ') : stmt.Action;
    return `${stmt.Effect} ${acts}`;
  };
  const preview = (doc.Statement ?? []).slice(0, 3).map(summarize).join('\n');
  const more =
    (doc.Statement ?? []).length > 3
      ? `\n… +${(doc.Statement ?? []).length - 3}`
      : '';

  return (
    <span
      className="font-mono text-xs tabular-nums"
      title={`${t('roles.policyMode')}\n${preview}${more}`}
    >
      ~{granted.size}
    </span>
  );
}

function ConstraintRow({
  label,
  items,
  resolveLabel,
}: {
  label: ReactNode;
  items: string[];
  resolveLabel: (s: string) => string;
}) {
  const { t } = useTranslation();
  return (
    <div>
      <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="mt-1 flex flex-wrap gap-1">
        {items.length === 0 ? (
          <span className="text-xs italic text-muted-foreground">{t('common.none')}</span>
        ) : (
          items.map((s) => (
            <Badge key={s} variant="outline" className="text-[10px]">
              {resolveLabel(s)}
            </Badge>
          ))
        )}
      </div>
    </div>
  );
}

function PolicyEditor({
  value,
  onChange,
  error,
  onApplyTemplate,
}: {
  value: string;
  onChange: (v: string) => void;
  error: string;
  onApplyTemplate: (tpl: PolicyDocument) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="space-y-3">
      <div>
        <Label className="text-sm font-medium">{t('roles.policyTemplates')}</Label>
        <div className="flex flex-wrap gap-1.5 mt-1.5">
          {Object.entries(POLICY_TEMPLATES).map(([key, tpl]) => (
            <Button
              key={key}
              variant="outline"
              size="sm"
              type="button"
              className="text-xs h-7"
              onClick={() => onApplyTemplate(tpl)}
            >
              <Copy className="mr-1 h-3 w-3" />
              {t(`roles.template_${key}` as const, { defaultValue: key })}
            </Button>
          ))}
        </div>
      </div>
      <div>
        <Label className="text-sm font-medium">{t('roles.policyDocument')}</Label>
        <p className="text-xs text-muted-foreground mb-1.5">{t('roles.policyDocumentDesc')}</p>
        <Textarea
          className="font-mono min-h-[260px] resize-y"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          spellCheck={false}
          placeholder={JSON.stringify(POLICY_TEMPLATES.developer, null, 2)}
        />
        {error && <p className="text-xs text-destructive mt-1">{error}</p>}
      </div>
    </div>
  );
}
