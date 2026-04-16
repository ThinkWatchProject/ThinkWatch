import {
  useEffect,
  useMemo,
  useRef,
  useState,
  useCallback,
  type FormEvent,
  type ReactNode,
} from 'react';
import { useTranslation } from 'react-i18next';
import { Card } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { RoleWizard } from '@/components/roles/RoleWizard';
import { useRoleForm, fromRoleResponse, emptyRoleForm, buildRolePayload } from '@/components/roles/useRoleForm';
import { PermissionTree } from '@/components/roles/PermissionTree';
import { StepBasics } from '@/components/roles/steps/StepBasics';
import { StepReview } from '@/components/roles/steps/StepReview';
import { RoleHistory } from '@/components/roles/RoleHistory';
import { RoleMembers } from '@/components/roles/RoleMembers';
import { RoleDetail } from '@/components/roles/RoleDetail';

/** Subset of the admin user-list row we need for the delete-reassign
 *  flow: PATCH /api/admin/users/{id} expects the full role_assignments
 *  array (replace-all semantics), so we read-modify-write it here. */
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
import type { ParsedConstraints } from './roles/types';
import { lazy, Suspense } from 'react';
// Lazy — pulls in codemirror + @codemirror/lang-json (~418 KB). Only
// users who toggle "Policy JSON" mode on a role ever pay the download.
const PolicyEditor = lazy(() => import('@/components/roles/PolicyEditor'));
function PolicyEditorFallback() {
  return <Skeleton className="h-[380px] w-full rounded-md" />;
}
import { toast } from 'sonner';
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
import {
  Shield,
  Plus,
  Pencil,
  Trash2,
  Search,
  AlertTriangle,
  Lock,
  Download,
  Upload,
} from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { ScrollArea } from '@/components/ui/scroll-area';
import { LimitsPanel } from '@/components/limits/limits-panel';
// Types, policy templates, and the simple↔policy conversion logic
// live in `roles/types.ts`. The page component itself is still
// large — owns every dialog, form, and member list — but the data
// shapes and pure helpers it consumes are now reusable and
// unit-testable in isolation.
import {
  groupByResource,
  type McpServer,
  type McpToolRow,
  type ModelRow,
  type PermissionDef,
  type PolicyDocument,
  type PolicyStatement,
  permsToPolicy,
  policyToPerms,
  type RoleMember,
  type RoleResponse as BaseRoleResponse,
} from './roles/types';

type RoleResponse = BaseRoleResponse;

// (Types, POLICY_TEMPLATES, SIMPLE_TEMPLATES, and the simple↔policy
// conversion helpers all live in `./roles/types.ts` — see the
// imports at the top of this file.)

// ----------------------------------------------------------------------------
// Page
// ----------------------------------------------------------------------------

export function RolesPage() {
  const { t } = useTranslation();
  const [roles, setRoles] = useState<RoleResponse[]>([]);
  const [permissions, setPermissions] = useState<PermissionDef[]>([]);
  const [availableModelRows, setAvailableModelRows] = useState<ModelRow[]>([]);
  const [availableServers, setAvailableServers] = useState<McpServer[]>([]);
  const [availableMcpTools, setAvailableMcpTools] = useState<McpToolRow[]>([]);
  // Team list — used to render scope badges as "team: engineering"
  // instead of the raw `team:<uuid>` the wire format carries.
  const [teamsById, setTeamsById] = useState<Map<string, { id: string; name: string }>>(
    new Map(),
  );
  const [loading, setLoading] = useState(true);

  // Filters
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState<'all' | 'system' | 'custom'>('all');

  // Create + edit forms — consolidated into `useRoleForm` hook.
  // Each hook owns one instance of the form state bag (name, perms,
  // model/tool scope, policy JSON, mode). The two instances stay
  // independent so the edit dialog never clobbers a half-typed create,
  // and closing either dialog doesn't reset the other.
  const createForm = useRoleForm();
  const editForm = useRoleForm();

  const [createOpen, setCreateOpen] = useState(false);
  const [creating, setCreating] = useState(false);
  const [createConstraints, setCreateConstraints] = useState<ParsedConstraints>({});
  const [editOpen, setEditOpen] = useState(false);
  const [editRole, setEditRole] = useState<RoleResponse | null>(null);
  // Edit-side limits buffer. Seeded from the role's inline
  // `surface_constraints` JSONB on openEdit and posted back verbatim
  // as part of the role PATCH body.
  const [editConstraints, setEditConstraints] = useState<ParsedConstraints>({});
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

  // Import flow: hidden file input and a busy flag while we POST one
  // role at a time. The actual file picker is mounted in the toolbar.
  const importInputRef = useRef<HTMLInputElement>(null);
  const [importing, setImporting] = useState(false);
  const [importResult, setImportResult] = useState<{
    created: number;
    skipped: number;
    failed: { name: string; reason: string }[];
  } | null>(null);

  // ------------------------------------------------------------------
  // Data fetch
  // ------------------------------------------------------------------

  const fetchData = useCallback(async () => {
    try {
      const [rolesRes, perms, modelsRes, serversRes, teamsRes, toolsRes] = await Promise.all([
        api<{ items: RoleResponse[] }>('/api/admin/roles'),
        api<PermissionDef[]>('/api/admin/permissions'),
        api<ModelRow[]>('/api/admin/models').catch(() => [] as ModelRow[]),
        api<McpServer[]>('/api/mcp/servers').catch(() => [] as McpServer[]),
        // Teams power the scope badge on member rows. team_managers
        // can read this endpoint too — they just see fewer teams.
        api<Array<{ id: string; name: string }>>('/api/admin/teams').catch(() => []),
        api<McpToolRow[]>('/api/mcp/tools').catch(() => [] as McpToolRow[]),
      ]);
      setRoles(rolesRes.items);
      setPermissions(perms);
      setAvailableModelRows(modelsRes);
      setAvailableServers(serversRes);
      setAvailableMcpTools(toolsRes);
      setTeamsById(new Map(teamsRes.map((t) => [t.id, t])));
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

  // Build a server-id → server-name lookup for MCP tools tree.
  const serverNameById = useMemo(
    () => new Map(availableServers.map((s) => [s.id, s.name])),
    [availableServers],
  );
  // server_id → namespace_prefix — authoritative ACL key used for the
  // `<prefix>__*` wildcard when "select all" is clicked on a server.
  // Sourced from /api/mcp/servers (always present) rather than reverse-
  // deriving from `namespaced_name` (would break when prefix contains
  // `__`).
  const serverPrefixById = useMemo(
    () => new Map(availableServers.map((s) => [s.id, s.namespace_prefix])),
    [availableServers],
  );

  // MCP tools grouped by server display name. Both name and prefix
  // come from the /api/mcp/servers response (serverNameById /
  // serverPrefixById) — tools without a corresponding server row are
  // skipped rather than falling back to server_id, which would leak
  // an unusable UUID into the picker.
  const mcpToolsByServer = useMemo(() => {
    const out = new Map<
      string,
      { serverName: string; prefix: string; tools: { key: string; toolName: string }[] }
    >();
    for (const tool of availableMcpTools) {
      const serverName = serverNameById.get(tool.server_id);
      const prefix = serverPrefixById.get(tool.server_id);
      if (!serverName || !prefix) continue; // server removed / out-of-sync
      let group = out.get(serverName);
      if (!group) {
        group = { serverName, prefix, tools: [] };
        out.set(serverName, group);
      }
      group.tools.push({
        key: tool.namespaced_name,
        toolName: tool.name,
      });
    }
    return out;
  }, [availableMcpTools, serverNameById, serverPrefixById]);

  // Models grouped by provider name.
  const modelsByProvider = useMemo(() => {
    const out = new Map<string, { modelId: string; displayName: string }[]>();
    for (const m of availableModelRows) {
      let group = out.get(m.provider_name);
      if (!group) {
        group = [];
        out.set(m.provider_name, group);
      }
      group.push({ modelId: m.model_id, displayName: m.display_name });
    }
    return out;
  }, [availableModelRows]);

  // System roles are locked in the UI: no edit button on the row,
  // no reset button in the dialog. The backend `roles:edit_system`
  // permission and the reset endpoint still exist (so we can flip
  // this back without a schema change), but exposing them via the
  // UI was confusing — operators expect "system role" to mean
  // "untouchable from this page". Use clone-from to fork instead.
  const canEditSystem = false;

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
  /// policy-mode roles too. Doesn't try to interpret Effect / Resource —
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
      const allPerms = extractPolicyActions(r.policy_document);
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
    form: ReturnType<typeof useRoleForm>,
    constraints: ParsedConstraints,
    setConstraints: (c: ParsedConstraints) => void,
  ) => {
    if (next === current) return;
    if (next === 'policy') {
      form.setPolicyJson(
        JSON.stringify(
          permsToPolicy(form.perms, form.models, form.mcpTools, constraints),
          null,
          2,
        ),
      );
      form.setPolicyError('');
      form.setMode('policy');
      return;
    }
    // policy → simple
    const result = policyToPerms(form.policyJson, permissions);
    if (result.parseError) {
      form.setPolicyError(t('roles.invalidJson'));
      return;
    }
    form.setPerms(result.perms);
    form.setModels(result.models);
    form.setMcpTools(result.mcpTools);
    setConstraints(result.constraints);
    form.setPolicyError(result.lossy ? t('roles.policySyncLossy') : '');
    form.setMode('simple');
  };

  const resetCreateForm = () => createForm.reset(emptyRoleForm());

  /// Compute the set of dangerous permission keys currently selected
  /// in the given perms set. Used to decide whether to gate the
  /// save behind a confirmation dialog.
  const dangerKeysIn = (set: Set<string>): string[] =>
    Array.from(set).filter((k) => dangerousKeys.has(k));

  const handleCreate = async (e?: FormEvent) => {
    e?.preventDefault();
    createForm.setPolicyError('');
    if (createForm.mode === 'policy' && createForm.policyJson.trim()) {
      try {
        JSON.parse(createForm.policyJson);
      } catch {
        createForm.setPolicyError(t('roles.invalidJson'));
        return;
      }
    }
    const performCreate = async () => {
      setCreating(true);
      try {
        const created = await apiPost<RoleResponse>('/api/admin/roles', {
          name: createForm.name,
          description: createForm.description || null,
          ...buildRolePayload(createForm, permissions, createConstraints),
        });
        setCreateOpen(false);
        resetCreateForm();
        setCreateConstraints({});
        await fetchData();
        toast.success(t('roles.createdSuccessfully', { name: created.name }));
        return;
      } catch {
        // surfaced via toast elsewhere; keep dialog open
      } finally {
        setCreating(false);
      }
    };

    // Only the simple-mode danger set is checked in the gate. Policy
    // mode is power-user territory and the policy doc may opt out via
    // explicit Deny rules; we don't try to second-guess it here.
    const danger = createForm.mode === 'simple' ? dangerKeysIn(createForm.perms) : [];
    if (danger.length > 0) {
      setDangerConfirm({ keys: danger, run: performCreate });
    } else {
      await performCreate();
    }
  };

  const openEdit = (r: RoleResponse) => {
    setEditRole(r);
    editForm.reset(fromRoleResponse(r, permissions));
    const parsed = policyToPerms(JSON.stringify(r.policy_document), permissions);
    setEditConstraints(parsed.constraints);
    setEditOpen(true);
  };

  const handleEdit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (!editRole) return;
    editForm.setPolicyError('');
    if (editForm.mode === 'policy' && editForm.policyJson.trim()) {
      try {
        JSON.parse(editForm.policyJson);
      } catch {
        editForm.setPolicyError(t('roles.invalidJson'));
        return;
      }
    }
    const performEdit = async () => {
      setSaving(true);
      try {
        await apiPatch(`/api/admin/roles/${editRole.id}`, {
          name: editForm.name,
          description: editForm.description || null,
          ...buildRolePayload(editForm, permissions, editConstraints),
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
    const oldParsed = policyToPerms(JSON.stringify(editRole.policy_document), permissions);
    const oldDanger = new Set([...oldParsed.perms].filter((k) => dangerousKeys.has(k)));
    const newDanger =
      editForm.mode === 'simple' ? dangerKeysIn(editForm.perms).filter((k) => !oldDanger.has(k)) : [];
    if (newDanger.length > 0) {
      setDangerConfirm({ keys: newDanger, run: performEdit });
    } else {
      await performEdit();
    }
  };

  // ----- Import / export -----------------------------------------
  //
  // The export shape is a small wrapper around the role rows so we
  // can version the format and include metadata like the source
  // hostname / export timestamp. Imported files are validated
  // against the expected fields; system roles are skipped on import
  // because they're seeded by the migration and would collide on
  // the unique name.

  type ExportedRole = {
    name: string;
    description: string | null;
    policy_document: PolicyDocument;
  };

  type ExportEnvelope = {
    version: 1;
    exported_at: string;
    roles: ExportedRole[];
  };

  const exportRoles = (allRoles: RoleResponse[]) => {
    const envelope: ExportEnvelope = {
      version: 1,
      exported_at: new Date().toISOString(),
      // System roles are skipped — they're seeded by the migration
      // on the destination side and importing them would collide
      // on the unique `name` constraint.
      roles: allRoles
        .filter((r) => !r.is_system)
        .map((r) => ({
          name: r.name,
          description: r.description,
          policy_document: r.policy_document,
        })),
    };
    const blob = new Blob([JSON.stringify(envelope, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `thinkwatch-roles-${new Date().toISOString().slice(0, 10)}.json`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  };

  const handleImport = async (file: File) => {
    setImporting(true);
    setImportResult(null);
    try {
      const text = await file.text();
      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        setImportResult({
          created: 0,
          skipped: 0,
          failed: [{ name: file.name, reason: t('roles.invalidJson') }],
        });
        return;
      }
      // Accept either the wrapped envelope or a bare array of roles
      // so admins can hand-author a tiny single-role file too.
      const incoming: ExportedRole[] = Array.isArray(parsed)
        ? (parsed as ExportedRole[])
        : ((parsed as ExportEnvelope)?.roles ?? []);
      if (incoming.length === 0) {
        setImportResult({
          created: 0,
          skipped: 0,
          failed: [{ name: file.name, reason: t('roles.importEmpty') }],
        });
        return;
      }
      const existingNames = new Set(roles.map((r) => r.name));
      const failed: { name: string; reason: string }[] = [];
      let created = 0;
      let skipped = 0;
      for (const r of incoming) {
        // Light validation. The backend will re-validate, but we
        // bail early on the obvious cases so the per-row error
        // attribution stays meaningful.
        if (!r || typeof r.name !== 'string' || !r.name.trim()) {
          failed.push({ name: r?.name ?? '?', reason: t('roles.importMissingName') });
          continue;
        }
        if (existingNames.has(r.name)) {
          skipped += 1;
          continue;
        }
        try {
          await apiPost('/api/admin/roles', {
            name: r.name,
            description: r.description ?? null,
            policy_document: r.policy_document ?? { Version: '2024-01-01', Statement: [] },
          });
          created += 1;
        } catch (e) {
          failed.push({
            name: r.name,
            reason: e instanceof Error ? e.message : 'Failed',
          });
        }
      }
      setImportResult({ created, skipped, failed });
      if (created > 0) await fetchData();
    } finally {
      setImporting(false);
    }
  };

  /// Reset a system role to its catalog defaults via the dedicated
  /// backend endpoint. Confirmation is delegated to a window.confirm
  /// because this is the rare case where we want to BLOCK the
  /// operator on a yes/no the simple way — wiring it through the
  /// danger-confirm dialog would obscure that this is a destructive
  /// reset of the entire role, not just the new permissions.
  const [resetConfirmOpen, setResetConfirmOpen] = useState(false);
  const handleResetSystemRole = async () => {
    if (!editRole) return;
    setResetConfirmOpen(false);
    setSaving(true);
    try {
      const updated = await apiPost<RoleResponse>(
        `/api/admin/roles/${editRole.id}/reset`,
        {},
      );
      editForm.reset(fromRoleResponse(updated));
      await fetchData();
    } catch {
      // surfaced via toast
    } finally {
      setSaving(false);
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
        <div className="flex items-center gap-2">
          {/* Export every CUSTOM role to a JSON file. System roles are
              skipped because they're seeded by the migration anyway and
              re-importing them would just collide on the unique name. */}
          <Button
            variant="outline"
            size="sm"
            onClick={() => exportRoles(roles)}
            disabled={roles.filter((r) => !r.is_system).length === 0}
          >
            <Download className="mr-1 h-3.5 w-3.5" />
            {t('roles.exportAll')}
          </Button>
          {/* Hidden file input + visible button. Imported file is
              parsed client-side, validated, and POSTed one role at a
              time so failures surface per-role. */}
          <input
            ref={importInputRef}
            type="file"
            accept="application/json,.json"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) handleImport(file);
              // Reset so the same filename can be re-picked.
              e.target.value = '';
            }}
          />
          <Button
            variant="outline"
            size="sm"
            onClick={() => importInputRef.current?.click()}
            disabled={importing}
          >
            <Upload className="mr-1 h-3.5 w-3.5" />
            {importing ? t('common.loading') : t('roles.importRoles')}
          </Button>
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
          <DialogContent className="w-[min(95vw,80rem)] max-w-none max-h-[90vh] overflow-y-auto flex flex-col sm:max-w-none">
            <DialogHeader>
              <DialogTitle>{t('roles.addRole')}</DialogTitle>
              <DialogDescription>{t('roles.addRoleDescription')}</DialogDescription>
            </DialogHeader>
            <RoleWizard
              submitting={creating}
              submitLabel={t('common.create')}
              onSubmit={() => void handleCreate()}
              steps={[
                {
                  id: 'basics',
                  label: t('roles.stepBasics'),
                  hint: t('roles.stepBasicsHint'),
                  validate: () => (!createForm.name.trim() ? t('roles.nameRequired') : null),
                  content: (
                    <StepBasics
                      mode="create"
                      name={createForm.name}
                      onNameChange={createForm.setName}
                      description={createForm.description}
                      onDescriptionChange={createForm.setDescription}
                      roles={roles}
                      permissions={permissions}
                      scopeState={{
                        perms: createForm.perms,
                        setPerms: createForm.setPerms,
                        models: createForm.models,
                        setModels: createForm.setModels,
                        mcpTools: createForm.mcpTools,
                        setMcpTools: createForm.setMcpTools,
                      }}
                    />
                  ),
                },
                {
                  id: 'permissions',
                  label: t('roles.stepPermissions'),
                  hint: t('roles.stepPermissionsHint'),
                  content: (
                    <div className="space-y-4">
                      <Tabs
                        value={createForm.mode}
                        onValueChange={(v) =>
                          switchMode(v as 'simple' | 'policy', createForm.mode, createForm, createConstraints, setCreateConstraints)
                        }
                      >
                        <TabsList className="grid w-full grid-cols-2">
                          <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                          <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                        </TabsList>
                        <TabsContent value="simple" className="mt-3">
                          <PermissionTree
                            grouped={grouped}
                            selected={createForm.perms}
                            onTogglePerm={(p) => togglePerm(createForm.perms, createForm.setPerms, p)}
                            onToggleGroup={(perms) =>
                              toggleResourceGroup(createForm.perms, createForm.setPerms, perms)
                            }
                            onSelectAll={() =>
                              createForm.setPerms(new Set(permissions.map((p) => p.key)))
                            }
                            onClear={() => createForm.setPerms(new Set())}
                            models={createForm.models}
                            onModelsChange={createForm.setModels}
                            modelsByProvider={modelsByProvider}
                            mcpTools={createForm.mcpTools}
                            onMcpToolsChange={createForm.setMcpTools}
                            mcpToolsByServer={mcpToolsByServer}
                            renderGroupExtra={(group) => {
                              const surface = surfaceFor(group);
                              if (!surface) return null;
                              if (!createForm.perms.has(`${surface}:use`)) return null;
                              return (
                                <SurfaceLimitsSlot>
                                  <LimitsPanel
                                    surfaces={[surface]}
                                    allowBudgets={surface === 'ai_gateway'}
                                    compact
                                    value={createConstraints}
                                    onChange={setCreateConstraints}
                                  />
                                </SurfaceLimitsSlot>
                              );
                            }}
                          />
                        </TabsContent>
                        <TabsContent value="policy" className="mt-3">
                          <Suspense fallback={<PolicyEditorFallback />}>
                            <PolicyEditor
                              value={createForm.policyJson}
                              onChange={createForm.setPolicyJson}
                              error={createForm.policyError}
                              onApplyTemplate={(tpl) =>
                                createForm.setPolicyJson(JSON.stringify(tpl, null, 2))
                              }
                            />
                          </Suspense>
                        </TabsContent>
                      </Tabs>
                      {createForm.mode === 'simple' && hasDangerous(createForm.perms, dangerousKeys) && (
                        <DangerPermissionWarning />
                      )}
                    </div>
                  ),
                },
                {
                  id: 'review',
                  label: t('roles.stepReview'),
                  hint: t('roles.stepReviewHint'),
                  content: (
                    <StepReview
                      name={createForm.name}
                      description={createForm.description}
                      mode={createForm.mode}
                      perms={createForm.perms}
                      policyJson={createForm.policyJson}
                      models={createForm.models}
                      mcpTools={createForm.mcpTools}
                      permissions={permissions}
                      dangerousKeys={dangerousKeys}
                    />
                  ),
                },
              ]}
            />
          </DialogContent>
        </Dialog>
        </div>
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
                    <PolicyPermSummary
                      doc={role.policy_document}
                      catalog={permissions}
                    />
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {role.user_count}
                  </TableCell>
                  <TableCell
                    className="text-right"
                    onClick={(e) => e.stopPropagation()}
                  >
                    <div className="flex justify-end gap-1">
                      {/* Edit is now allowed on system roles too,
                          gated by `roles:edit_system`. Delete stays
                          custom-only — system roles are immortal. */}
                      {(!role.is_system || canEditSystem) && (
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7"
                          onClick={() => openEdit(role)}
                          aria-label={t('common.edit')}
                        >
                          <Pencil className="h-3.5 w-3.5" />
                        </Button>
                      )}
                      {!role.is_system && (
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7 text-destructive"
                          onClick={() => openDelete(role)}
                          aria-label={t('common.delete')}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </Button>
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
              catalog={permissions}
              dangerousKeys={dangerousKeys}
              availableServers={availableServers}
              teamsById={teamsById}
              onMembersChanged={fetchData}
            />
          )}
        </DialogContent>
      </Dialog>

      {/* Edit dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="w-[min(95vw,80rem)] max-w-none max-h-[90vh] overflow-y-auto flex flex-col sm:max-w-none">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              {t('roles.editRole')}
              {editRole?.is_system && (
                <Badge variant="secondary" className="text-[10px]">
                  {t('roles.systemRole')}
                </Badge>
              )}
            </DialogTitle>
            {editRole?.is_system && (
              <DialogDescription>{t('roles.editSystemWarning')}</DialogDescription>
            )}
          </DialogHeader>
          <RoleWizard
            key={editRole?.id ?? 'edit'}
            submitting={saving}
            submitLabel={t('common.save')}
            onSubmit={() => void handleEdit()}
            footerExtras={
              editRole?.is_system ? (
                <Button
                  type="button"
                  variant="outline"
                  disabled={saving}
                  onClick={() => setResetConfirmOpen(true)}
                >
                  {t('roles.resetToDefaults')}
                </Button>
              ) : undefined
            }
            steps={[
              {
                id: 'basics',
                label: t('roles.stepBasics'),
                hint: t('roles.stepBasicsHint'),
                validate: () => (!editForm.name.trim() ? t('roles.nameRequired') : null),
                content: (
                  <StepBasics
                    mode="edit"
                    name={editForm.name}
                    onNameChange={editForm.setName}
                    description={editForm.description}
                    onDescriptionChange={editForm.setDescription}
                    nameDisabled={editRole?.is_system}
                    metadata={
                      editRole
                        ? {
                            created_at: editRole.created_at,
                            updated_at: editRole.updated_at,
                            created_by_email: editRole.created_by_email,
                          }
                        : undefined
                    }
                  />
                ),
              },
              {
                id: 'permissions',
                label: t('roles.stepPermissions'),
                hint: t('roles.stepPermissionsHint'),
                content: (
                  <div className="space-y-4">
                    <Tabs
                      value={editForm.mode}
                      onValueChange={(v) =>
                        switchMode(v as 'simple' | 'policy', editForm.mode, editForm, editConstraints, setEditConstraints)
                      }
                    >
                      <TabsList className="grid w-full grid-cols-2">
                        <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                        <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                      </TabsList>
                      <TabsContent value="simple" className="mt-3">
                        <PermissionTree
                          grouped={grouped}
                          selected={editForm.perms}
                          onTogglePerm={(p) => togglePerm(editForm.perms, editForm.setPerms, p)}
                          onToggleGroup={(perms) =>
                            toggleResourceGroup(editForm.perms, editForm.setPerms, perms)
                          }
                          onSelectAll={() => editForm.setPerms(new Set(permissions.map((p) => p.key)))}
                          onClear={() => editForm.setPerms(new Set())}
                          models={editForm.models}
                          onModelsChange={editForm.setModels}
                          modelsByProvider={modelsByProvider}
                          mcpTools={editForm.mcpTools}
                          onMcpToolsChange={editForm.setMcpTools}
                          mcpToolsByServer={mcpToolsByServer}
                          renderGroupExtra={(group) => {
                            const surface = surfaceFor(group);
                            if (!surface) return null;
                            if (!editForm.perms.has(`${surface}:use`)) return null;
                            return (
                              <SurfaceLimitsSlot>
                                <LimitsPanel
                                  surfaces={[surface]}
                                  allowBudgets={surface === 'ai_gateway'}
                                  compact
                                  value={editConstraints}
                                  onChange={setEditConstraints}
                                />
                              </SurfaceLimitsSlot>
                            );
                          }}
                        />
                      </TabsContent>
                      <TabsContent value="policy" className="mt-3">
                        <Suspense fallback={<PolicyEditorFallback />}>
                          <PolicyEditor
                            value={editForm.policyJson}
                            onChange={editForm.setPolicyJson}
                            error={editForm.policyError}
                            onApplyTemplate={(tpl) =>
                              editForm.setPolicyJson(JSON.stringify(tpl, null, 2))
                            }
                          />
                        </Suspense>
                      </TabsContent>
                    </Tabs>
                    {editForm.mode === 'simple' && hasDangerous(editForm.perms, dangerousKeys) && (
                      <DangerPermissionWarning />
                    )}
                  </div>
                ),
              },
              {
                id: 'members',
                label: t('roles.stepMembers'),
                hint: t('roles.stepMembersHint'),
                content: editRole ? (
                  <div className="space-y-3">
                    <p className="text-[11px] italic text-muted-foreground">
                      {t('roles.membersImmediateNote')}
                    </p>
                    <RoleMembers
                      role={editRole}
                      teamsById={teamsById}
                      onMembersChanged={fetchData}
                    />
                  </div>
                ) : null,
              },
              {
                id: 'history',
                label: t('roles.stepHistory'),
                hint: t('roles.stepHistoryHint'),
                content: editRole ? <RoleHistory roleId={editRole.id} /> : null,
              },
              {
                id: 'review',
                label: t('roles.stepReview'),
                hint: t('roles.stepReviewHint'),
                content: (
                  <StepReview
                    name={editForm.name}
                    description={editForm.description}
                    mode={editForm.mode}
                    perms={editForm.perms}
                    policyJson={editForm.policyJson}
                    models={editForm.models}
                    mcpTools={editForm.mcpTools}
                    permissions={permissions}
                    dangerousKeys={dangerousKeys}
                  />
                ),
              },
            ]}
          />
        </DialogContent>
      </Dialog>

      {/* Reset system role to defaults — destructive confirm */}
      <ConfirmDialog
        open={resetConfirmOpen}
        onOpenChange={setResetConfirmOpen}
        title={t('roles.resetToDefaults')}
        description={
          editRole ? t('roles.resetSystemConfirm', { name: editRole.name }) : ''
        }
        variant="destructive"
        confirmLabel={t('roles.resetToDefaults')}
        onConfirm={handleResetSystemRole}
        loading={saving}
      />

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
                                  {(() => {
                                    const teamId = m.scope.startsWith('team:')
                                      ? m.scope.slice(5)
                                      : '';
                                    const team = teamsById.get(teamId);
                                    return team
                                      ? `${t('users.scopeTeam')}: ${team.name}`
                                      : m.scope;
                                  })()}
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

      {/* Import result summary. Shows created / skipped / failed
          counts so the admin can see what happened to each role
          in a multi-row import file. */}
      <Dialog
        open={!!importResult}
        onOpenChange={(o) => !o && setImportResult(null)}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('roles.importResultTitle')}</DialogTitle>
          </DialogHeader>
          {importResult && (
            <div className="space-y-2 text-sm">
              <div className="flex justify-between">
                <span>{t('roles.importCreated')}</span>
                <span className="font-mono tabular-nums">{importResult.created}</span>
              </div>
              <div className="flex justify-between">
                <span>{t('roles.importSkipped')}</span>
                <span className="font-mono tabular-nums">{importResult.skipped}</span>
              </div>
              <div className="flex justify-between">
                <span>{t('roles.importFailed')}</span>
                <span className="font-mono tabular-nums text-destructive">
                  {importResult.failed.length}
                </span>
              </div>
              {importResult.failed.length > 0 && (
                <ScrollArea className="max-h-40 rounded-md border">
                  <ul className="divide-y text-xs">
                    {importResult.failed.map((f, i) => (
                      <li key={i} className="px-2 py-1.5">
                        <div className="font-mono">{f.name}</div>
                        <div className="text-[10px] text-destructive">{f.reason}</div>
                      </li>
                    ))}
                  </ul>
                </ScrollArea>
              )}
            </div>
          )}
          <DialogFooter>
            <Button onClick={() => setImportResult(null)}>{t('common.done')}</Button>
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

/// Map a permission-tree resource group to the gateway surface whose
/// rate limits / budgets are meaningful there. Returns `null` for
/// resource groups that don't carry surface-scoped constraints.
function surfaceFor(group: string): 'ai_gateway' | 'mcp_gateway' | null {
  if (group === 'ai_gateway') return 'ai_gateway';
  if (group === 'mcp_gateway') return 'mcp_gateway';
  return null;
}

function SurfaceLimitsSlot({ children }: { children: ReactNode }) {
  const { t } = useTranslation();
  return (
    <div className="mt-3 space-y-2 border-t pt-3">
      <div className="text-xs font-medium">{t('roles.surfaceLimitsHeader')}</div>
      {children}
    </div>
  );
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

/// Estimate the effective permission count for a policy_document by
/// expanding wildcard Allow actions against the static catalog and
/// subtracting Deny matches. Used in the role list to show a single
/// number for policy-mode roles instead of just "policy", and to render
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


