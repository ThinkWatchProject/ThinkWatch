import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
  DialogTrigger,
} from '@/components/ui/dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Plus, MoreHorizontal, Pencil, Trash2, LogOut as LogOutIcon, KeyRound, Ban, CheckCircle, Users as UsersIcon, AlertCircle, Copy, Search, ChevronRight, ChevronDown, X } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import type { TeamSummary } from '@/lib/types';
import { useTeams } from '@/hooks/use-teams';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { policyToPerms, type PermissionDef, type PolicyDocument } from './roles/types';
import { UserLimitsTab } from '@/components/limits/user-limits-tab';
import { BulkOverrideDialog } from '@/components/limits/bulk-override-dialog';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { Checkbox } from '@/components/ui/checkbox';
import { toast } from 'sonner';
import { Gauge } from 'lucide-react';


interface RoleAssignment {
  role_id: string;
  name: string;
  is_system: boolean;
  scope: string;
}

interface User {
  id: string;
  email: string;
  display_name: string;
  role_assignments: RoleAssignment[];
  /// Teams this user belongs to. Used by the team column on the
  /// admin user list so a team_manager looking at a merged
  /// multi-team result set can tell which row belongs where.
  teams?: Array<{ id: string; name: string }>;
  oidc_subject: string | null;
  is_active: boolean;
  created_at: string;
}

interface AvailableRole {
  id: string;
  name: string;
  is_system: boolean;
  description: string | null;
  policy_document: PolicyDocument;
}

export function UsersPage() {
  const { t } = useTranslation();
  const [users, setUsers] = useState<User[]>([]);
  const [availableRoles, setAvailableRoles] = useState<AvailableRole[]>([]);
  const [availablePermissions, setAvailablePermissions] = useState<PermissionDef[]>([]);
  const { teams: availableTeams } = useTeams();
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');
  // Server-side pagination. `debouncedSearch` feeds the API so fast
  // typing doesn't fan out one request per keystroke.
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(20);
  const [total, setTotal] = useState(0);
  // Global view of who currently holds the super_admin role. Needed
  // to disable destructive row actions on whoever is the sole holder,
  // since the paginated user list can't see users on other pages.
  // Refetched alongside the users list so it stays in sync with
  // delete / disable / role-change mutations.
  const [superAdminIds, setSuperAdminIds] = useState<Set<string>>(new Set());

  // Create dialog
  const [createOpen, setCreateOpen] = useState(false);
  const [bulkOverrideOpen, setBulkOverrideOpen] = useState(false);
  // Active tab in the user-edit dialog. "limits" tab hides the save
  // footer because limits edits commit via their own inline actions.
  const [editTab, setEditTab] = useState<'basic' | 'access' | 'limits'>('basic');
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [createAssignments, setCreateAssignments] = useState<RoleAssignment[]>([]);

  // Edit dialog
  const [editOpen, setEditOpen] = useState(false);
  const [editUser, setEditUser] = useState<User | null>(null);
  const [editName, setEditName] = useState('');
  const [editError, setEditError] = useState('');
  const [editLoading, setEditLoading] = useState(false);
  const [editAssignments, setEditAssignments] = useState<RoleAssignment[]>([]);
  const [editTeamIds, setEditTeamIds] = useState<string[]>([]);

  // Confirm dialogs
  const [confirmAction, setConfirmAction] = useState<{ type: 'logout' | 'delete' | 'toggle'; user: User } | null>(null);
  // Multi-select bulk actions. Selection is keyed by user id and is
  // purposefully NOT cleared on page change — an operator might page
  // to pick up more users of the same bucket before acting. We DO
  // drop ids that no longer exist in the current list (deleted
  // users, search filter, etc.) so the counter doesn't drift.
  const [selectedUserIds, setSelectedUserIds] = useState<Set<string>>(new Set());
  const [bulkAction, setBulkAction] = useState<'activate' | 'deactivate' | 'delete' | null>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const [confirmLoading, setConfirmLoading] = useState(false);
  const [confirmError, setConfirmError] = useState('');

  // Reset password dialog
  const [resetResult, setResetResult] = useState<{ password: string; userId: string } | null>(null);
  const [resetConfirmUser, setResetConfirmUser] = useState<User | null>(null);
  const [resetLoading, setResetLoading] = useState(false);

  const fetchUsers = async (signal?: AbortSignal) => {
    try {
      const params = new URLSearchParams({
        page: String(page),
        per_page: String(pageSize),
      });
      if (debouncedSearch) params.set('search', debouncedSearch);
      const [usersRes, rolesRes, permsRes, superRes] = await Promise.all([
        api<{ data: User[]; total: number }>(`/api/admin/users?${params.toString()}`, { signal }),
        // Roles list is small and only needed for the picker; fetch
        // once on mount, not on every page/search change.
        availableRoles.length === 0
          ? api<{ items: AvailableRole[] }>('/api/admin/roles', { signal }).catch(() => ({ items: [] }))
          : Promise.resolve({ items: availableRoles }),
        // Permissions catalog is needed by policyToPerms to expand
        // wildcards and validate action keys in the effective-permissions
        // preview. Also fetch once.
        availablePermissions.length === 0
          ? api<PermissionDef[]>('/api/admin/permissions', { signal }).catch(() => [] as PermissionDef[])
          : Promise.resolve(availablePermissions),
        // Global super-admin id set, refetched every load so the row
        // action disable state stays accurate after any mutation.
        api<{ ids: string[] }>('/api/admin/users/super-admin-ids', { signal })
          .catch(() => ({ ids: [] as string[] })),
      ]);
      setUsers(
        usersRes.data.map((u) => ({
          ...u,
          role_assignments: u.role_assignments ?? [],
        })),
      );
      setTotal(usersRes.total);
      setSuperAdminIds(new Set(superRes.ids));
      // Unified picker — system + custom roles all show up together.
      if (availableRoles.length === 0) setAvailableRoles(rolesRes.items);
      if (availablePermissions.length === 0) setAvailablePermissions(permsRes);
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : 'Failed to load users');
    } finally {
      setLoading(false);
    }
  };

  /// Would a delete / disable / role-strip on this user drop the
  /// platform's super-admin quorum to zero? Mirrors the backend
  /// `assert_super_admin_quorum` check so the UI can grey the action
  /// out instead of letting the user click and get a 400. The backend
  /// remains authoritative.
  const isLastSuperAdmin = (u: User): boolean =>
    superAdminIds.has(u.id) && superAdminIds.size === 1;

  // Debounce the search box so we don't spam the backend on every
  // keystroke. 250ms matches what most admin panels use.
  useEffect(() => {
    const h = setTimeout(() => setDebouncedSearch(search.trim()), 250);
    return () => clearTimeout(h);
  }, [search]);

  // Typing a new search term should always land on page 1 — otherwise
  // you'd type "adm" and land on page 4 of a filtered result that
  // only has 2 pages.
  useEffect(() => {
    setPage(1);
  }, [debouncedSearch]);

  useEffect(() => {
    const controller = new AbortController();
    fetchUsers(controller.signal);
    return () => controller.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [page, pageSize, debouncedSearch]);

  // --- Create user ---
  const resetCreateForm = () => {
    setEmail('');
    setDisplayName('');
    setPassword('');
    // Seed new users with the `developer` system role when the catalog
    // is loaded — a user with zero assignments has zero permissions
    // and would look broken in the UI.
    const dev = availableRoles.find((r) => r.is_system && r.name === 'developer');
    setCreateAssignments(
      dev
        ? [{ role_id: dev.id, name: dev.name, is_system: true, scope: 'global' }]
        : [],
    );
    setFormError('');
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError(''); setSubmitting(true);
    try {
      await apiPost('/api/admin/users', {
        email,
        display_name: displayName,
        password,
        role_assignments: createAssignments.map((a) => ({
          role_id: a.role_id,
          scope: a.scope,
        })),
      });
      setCreateOpen(false); resetCreateForm(); await fetchUsers();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed');
    } finally { setSubmitting(false); }
  };

  // --- Edit user ---
  const openEdit = (u: User) => {
    setEditUser(u);
    setEditName(u.display_name);
    setEditAssignments(u.role_assignments ?? []);
    setEditTeamIds((u.teams ?? []).map((t) => t.id));
    setEditError('');
    setEditOpen(true);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!editUser) return;
    setEditLoading(true); setEditError('');
    try {
      await apiPatch(`/api/admin/users/${editUser.id}`, {
        display_name: editName,
        role_assignments: editAssignments.map((a) => ({
          role_id: a.role_id,
          scope: a.scope,
        })),
      });
      // Sync team memberships: add new, remove old
      const oldTeamIds = new Set((editUser.teams ?? []).map((t) => t.id));
      const newTeamIds = new Set(editTeamIds);
      for (const tid of editTeamIds) {
        if (!oldTeamIds.has(tid)) {
          await apiPost(`/api/admin/teams/${tid}/members`, { user_id: editUser.id });
        }
      }
      for (const tid of oldTeamIds) {
        if (!newTeamIds.has(tid)) {
          await apiDelete(`/api/admin/teams/${tid}/members/${editUser.id}`);
        }
      }
      setEditOpen(false); setEditUser(null); await fetchUsers();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed');
    } finally { setEditLoading(false); }
  };

  // --- Confirm actions (logout / delete / toggle active) ---
  const handleConfirm = async () => {
    if (!confirmAction) return;
    setConfirmLoading(true); setConfirmError('');
    try {
      const { type, user } = confirmAction;
      if (type === 'logout') {
        await apiPost(`/api/admin/users/${user.id}/force-logout`, {});
      } else if (type === 'delete') {
        await apiDelete(`/api/admin/users/${user.id}`);
      } else if (type === 'toggle') {
        await apiPatch(`/api/admin/users/${user.id}`, { is_active: !user.is_active });
      }
      setConfirmAction(null); await fetchUsers();
    } catch (err) {
      setConfirmError(err instanceof Error ? err.message : 'Failed');
    } finally { setConfirmLoading(false); }
  };

  // --- Bulk actions (activate / deactivate / delete) ---
  // Per-user HTTP calls dispatched in parallel via Promise.allSettled.
  // We don't batch into a single endpoint because the action-to-
  // endpoint mapping is 1:1 with existing /api/admin/users/{id}
  // routes — reusing them means the audit trail, permission gates,
  // and validation stay on the existing code paths unchanged.
  const runBulkAction = async (action: 'activate' | 'deactivate' | 'delete') => {
    const ids = Array.from(selectedUserIds);
    if (ids.length === 0) return;

    // Quorum pre-check: refuse bulk delete / disable that would drain
    // all active super admins. Backend enforces the same invariant so
    // the operator can't sneak around this, but catching it here
    // means we don't partially succeed (2 of 3 deletes commit before
    // the 3rd hits the 400) — cleaner failure mode.
    if (action === 'delete' || action === 'deactivate') {
      const selectedSupers = ids.filter((id) => superAdminIds.has(id)).length;
      const remainingSupers = superAdminIds.size - selectedSupers;
      if (remainingSupers < 1) {
        toast.error(t('users.guard.bulkLastSuperAdmin'));
        return;
      }
    }

    setBulkBusy(true);
    const call = (id: string): Promise<void> => {
      if (action === 'delete') return apiDelete(`/api/admin/users/${id}`);
      return apiPatch(`/api/admin/users/${id}`, {
        is_active: action === 'activate',
      });
    };
    const results = await Promise.allSettled(ids.map(call));
    const ok = results.filter((r) => r.status === 'fulfilled').length;
    const fail = results.length - ok;
    if (fail === 0) {
      toast.success(t('users.bulk.allOk', { count: ok, action: t(`users.bulk.action_${action}` as const) }));
    } else {
      // Surface the first failure's message so operators know what
      // to fix, not just a count.
      const firstErr = results.find((r) => r.status === 'rejected') as
        | PromiseRejectedResult
        | undefined;
      toast.warning(
        t('users.bulk.partial', {
          ok,
          fail,
          reason:
            firstErr?.reason instanceof Error
              ? firstErr.reason.message
              : String(firstErr?.reason ?? ''),
        }),
      );
    }
    setSelectedUserIds(new Set());
    setBulkAction(null);
    setBulkBusy(false);
    await fetchUsers();
  };

  // --- Reset password ---
  const handleResetPassword = async () => {
    if (!resetConfirmUser) return;
    setResetLoading(true);
    try {
      const res = await apiPost<{ temporary_password: string; user_id: string }>(
        `/api/admin/users/${resetConfirmUser.id}/reset-password`, {},
      );
      setResetResult({ password: res.temporary_password, userId: res.user_id });
      setResetConfirmUser(null);
    } catch (err) {
      setConfirmError(err instanceof Error ? err.message : 'Failed');
    } finally { setResetLoading(false); }
  };

  const confirmTitle = () => {
    if (!confirmAction) return '';
    if (confirmAction.type === 'logout') return t('users.forceLogout');
    if (confirmAction.type === 'delete') return t('users.deleteUser');
    return confirmAction.user.is_active ? t('users.deactivate') : t('users.activate');
  };

  const confirmDesc = () => {
    if (!confirmAction) return '';
    if (confirmAction.type === 'logout') return t('users.forceLogoutConfirm');
    if (confirmAction.type === 'delete') return t('users.deleteConfirm');
    return confirmAction.user.is_active ? t('users.deactivateConfirm') : t('users.activateConfirm');
  };

  /// Localized label for the canonical system role names. Falls back
  /// to the raw role name for custom roles so the operator sees exactly
  /// what they typed.
  const roleLabel = (r: string) => {
    const map: Record<string, string> = {
      super_admin: t('users.roleSuperAdmin'),
      admin: t('users.roleAdmin'),
      team_manager: t('users.roleTeamManager'),
      developer: t('users.roleDeveloper'),
      viewer: t('users.roleViewer'),
    };
    return map[r] ?? r;
  };

  // Server handles filtering (email + display_name ILIKE). Keep the
  // alias so the JSX below stays readable.
  const filteredUsers = users;

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('users.title')}</h1>
          <p className="text-muted-foreground">{t('users.subtitle')}</p>
        </div>
        <div className="flex items-center gap-2">
          <Dialog open={createOpen} onOpenChange={(open) => { setCreateOpen(open); if (!open) resetCreateForm(); }}>
            <DialogTrigger asChild>
              <Button><Plus className="h-4 w-4" />{t('users.addUser')}</Button>
            </DialogTrigger>
          <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
            <DialogHeader>
              <DialogTitle>{t('users.addUser')}</DialogTitle>
              <DialogDescription>{t('users.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-4">
              {formError && <Alert variant="destructive"><AlertCircle className="h-4 w-4" /><AlertDescription>{formError}</AlertDescription></Alert>}
              <div className="space-y-2">
                <Label htmlFor="user-email">{t('auth.email')}</Label>
                <Input id="user-email" type="email" value={email} onChange={(e) => setEmail(e.target.value)} placeholder="user@company.com" required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="user-name">{t('users.displayName')}</Label>
                <Input id="user-name" value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="John Doe" />
              </div>
              <div className="space-y-2">
                <Label htmlFor="user-password">{t('auth.password')}</Label>
                <Input id="user-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} required />
              </div>
              <RoleAssignmentEditor
                value={createAssignments}
                onChange={setCreateAssignments}
                availableRoles={availableRoles}
                availableTeams={availableTeams}
                roleLabel={roleLabel}
              />
              <EffectivePermissionsPreview
                assignments={createAssignments}
                availableRoles={availableRoles}
                availablePermissions={availablePermissions}
              />
              <DialogFooter>
                <Button type="submit" disabled={submitting}>{submitting ? t('users.creating') : t('users.createUser')}</Button>
              </DialogFooter>
            </form>
          </DialogContent>
          </Dialog>
        </div>
      </div>

      <BulkOverrideDialog
        open={bulkOverrideOpen}
        onOpenChange={setBulkOverrideOpen}
        targetUserIds={Array.from(selectedUserIds)}
        // Build the lookup lazily so it only exists while the dialog
        // is open — avoids a stale reference in memory on every render.
        userLookup={
          bulkOverrideOpen
            ? new Map(
                users.map((u) => [
                  u.id,
                  { email: u.email, display_name: u.display_name },
                ]),
              )
            : undefined
        }
      />

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="flex items-center gap-2 mb-4">
        <div className="relative w-full max-w-sm">
          <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('users.searchPlaceholder')}
            className="pl-8"
          />
        </div>
      </div>

      {/* Selection action bar — only rendered when at least one row is
          selected. Sits above the table so the operator can act without
          scrolling back to the top. */}
      {selectedUserIds.size > 0 && (
        <div className="mb-2 flex items-center justify-between rounded-md border bg-muted/40 px-3 py-2 text-sm">
          <span>
            {t('users.bulk.selected', { count: selectedUserIds.size })}
          </span>
          <div className="flex items-center gap-1.5">
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => setBulkAction('activate')}
              disabled={bulkBusy}
            >
              <CheckCircle className="mr-1 h-3.5 w-3.5" />
              {t('users.bulk.enable')}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => setBulkAction('deactivate')}
              disabled={bulkBusy}
            >
              <Ban className="mr-1 h-3.5 w-3.5" />
              {t('users.bulk.disable')}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="destructive"
              onClick={() => setBulkAction('delete')}
              disabled={bulkBusy}
            >
              <Trash2 className="mr-1 h-3.5 w-3.5" />
              {t('users.bulk.delete')}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => setBulkOverrideOpen(true)}
              disabled={bulkBusy}
            >
              <Gauge className="mr-1 h-3.5 w-3.5" />
              {t('users.bulk.applyOverride')}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => setSelectedUserIds(new Set())}
              disabled={bulkBusy}
            >
              {t('users.bulk.clearSelection')}
            </Button>
          </div>
        </div>
      )}

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-3 p-4">
              {[...Array(4)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-40" /><Skeleton className="h-4 w-24" /><Skeleton className="h-5 w-16 rounded-full" /><Skeleton className="h-5 w-14 rounded-full" /><Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : users.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center text-center">
              <UsersIcon className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">
                {debouncedSearch ? t('users.noMatches') : t('users.noUsers')}
              </p>
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead className="w-8">
                    <Checkbox
                      aria-label={t('users.bulk.selectAllVisible')}
                      checked={
                        filteredUsers.length > 0 &&
                        filteredUsers.every((u) => selectedUserIds.has(u.id))
                      }
                      onCheckedChange={(v) => {
                        setSelectedUserIds((prev) => {
                          const next = new Set(prev);
                          if (v) {
                            filteredUsers.forEach((u) => next.add(u.id));
                          } else {
                            filteredUsers.forEach((u) => next.delete(u.id));
                          }
                          return next;
                        });
                      }}
                    />
                  </TableHead>
                  <TableHead>{t('users.userId')}</TableHead>
                  <TableHead>{t('auth.email')}</TableHead>
                  <TableHead>{t('users.displayName')}</TableHead>
                  <TableHead>{t('users.roles')}</TableHead>
                  <TableHead>{t('users.teams')}</TableHead>
                  <TableHead>{t('users.sso')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('users.created')}</TableHead>
                  <TableHead className="w-12" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {filteredUsers.map((u) => (
                  <TableRow key={u.id} className={!u.is_active ? 'opacity-50' : undefined}>
                    <TableCell className="w-8">
                      <Checkbox
                        aria-label={t('users.bulk.selectRow')}
                        checked={selectedUserIds.has(u.id)}
                        onCheckedChange={() => {
                          setSelectedUserIds((prev) => {
                            const next = new Set(prev);
                            if (next.has(u.id)) next.delete(u.id);
                            else next.add(u.id);
                            return next;
                          });
                        }}
                      />
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <span
                          className="font-mono text-xs text-muted-foreground"
                          title={u.id}
                        >
                          {u.id.slice(0, 8)}
                        </span>
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-6 w-6"
                          onClick={() => navigator.clipboard.writeText(u.id)}
                          aria-label={t('users.copyId')}
                          title={t('users.copyId')}
                        >
                          <Copy className="h-3 w-3" />
                        </Button>
                      </div>
                    </TableCell>
                    <TableCell className="font-medium">{u.email}</TableCell>
                    <TableCell>{u.display_name || '—'}</TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {u.role_assignments.length === 0 && (
                          <span className="text-xs italic text-muted-foreground">
                            {t('common.none')}
                          </span>
                        )}
                        {u.role_assignments.map((a) => (
                          <Badge
                            key={`${a.role_id}-${a.scope}`}
                            variant={a.is_system ? 'secondary' : 'outline'}
                            className={a.is_system ? undefined : 'font-mono text-[10px]'}
                            title={a.scope !== 'global' ? `${a.name} @ ${a.scope}` : a.name}
                          >
                            {a.is_system ? roleLabel(a.name) : a.name}
                            {a.scope !== 'global' && (
                              <span className="ml-1 opacity-60">@{a.scope}</span>
                            )}
                          </Badge>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {(u.teams ?? []).length === 0 ? (
                          <span className="text-xs italic text-muted-foreground">
                            {t('common.none')}
                          </span>
                        ) : (
                          (u.teams ?? []).map((tm) => (
                            <Badge
                              key={tm.id}
                              variant="outline"
                              className="text-[10px]"
                              title={tm.name}
                            >
                              {tm.name}
                            </Badge>
                          ))
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      {u.oidc_subject ? <Badge variant="outline">OIDC</Badge> : <span className="text-xs text-muted-foreground">—</span>}
                    </TableCell>
                    <TableCell>
                      <Badge variant={u.is_active ? 'default' : 'destructive'}>
                        {u.is_active ? t('common.active') : t('common.inactive')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">{new Date(u.created_at).toLocaleDateString()}</TableCell>
                    <TableCell>
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button variant="ghost" size="icon" className="h-8 w-8">
                            <MoreHorizontal className="h-4 w-4" />
                          </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                          <DropdownMenuItem onClick={() => openEdit(u)}>
                            <Pencil className="h-4 w-4 mr-2" />{t('users.editUser')}
                          </DropdownMenuItem>
                          <DropdownMenuItem onClick={() => setResetConfirmUser(u)}>
                            <KeyRound className="h-4 w-4 mr-2" />{t('users.resetPassword')}
                          </DropdownMenuItem>
                          {/* Disable toggle + delete when this user is the
                              sole remaining active super admin — the
                              backend would reject the request anyway
                              (see `assert_super_admin_quorum`), but
                              surfacing it at the menu item level is
                              more honest than a toast after the click.
                              Disabling is also blocked because setting
                              is_active=false on the last super admin
                              violates the same invariant. */}
                          <DropdownMenuItem
                            disabled={u.is_active && isLastSuperAdmin(u)}
                            onClick={() => setConfirmAction({ type: 'toggle', user: u })}
                            title={
                              u.is_active && isLastSuperAdmin(u)
                                ? t('users.guard.lastSuperAdmin')
                                : undefined
                            }
                          >
                            {u.is_active
                              ? <><Ban className="h-4 w-4 mr-2" />{t('users.deactivate')}</>
                              : <><CheckCircle className="h-4 w-4 mr-2" />{t('users.activate')}</>}
                          </DropdownMenuItem>
                          <DropdownMenuItem onClick={() => setConfirmAction({ type: 'logout', user: u })}>
                            <LogOutIcon className="h-4 w-4 mr-2" />{t('users.forceLogout')}
                          </DropdownMenuItem>
                          <DropdownMenuSeparator />
                          <DropdownMenuItem
                            className="text-destructive"
                            disabled={isLastSuperAdmin(u)}
                            onClick={() => setConfirmAction({ type: 'delete', user: u })}
                            title={
                              isLastSuperAdmin(u)
                                ? t('users.guard.lastSuperAdmin')
                                : undefined
                            }
                          >
                            <Trash2 className="h-4 w-4 mr-2" />{t('users.deleteUser')}
                          </DropdownMenuItem>
                        </DropdownMenuContent>
                      </DropdownMenu>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        <div data-slot="card-footer" className="border-t">
          <DataTablePagination
            total={total}
            page={page}
            pageSize={pageSize}
            onPageChange={setPage}
            onPageSizeChange={setPageSize}
          />
        </div>
      </Card>

      {/* Edit dialog — three tabs:
          · basic   — email + display name
          · access  — role assignments + teams + effective perms
          · limits  — the new UserLimitsTab (effective policy + usage + audit)
          The form wraps the basic/access tabs so Save commits both.
          Limits has its own inline actions so the footer is hidden
          there. */}
      <Dialog
        open={editOpen}
        onOpenChange={(open) => {
          setEditOpen(open);
          if (!open) {
            setEditUser(null);
            setEditTab('basic');
          }
        }}
      >
        <DialogContent className="sm:max-w-3xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('users.editUser')}</DialogTitle>
            <DialogDescription>{t('users.editDescription')}</DialogDescription>
          </DialogHeader>
          <Tabs value={editTab} onValueChange={(v) => setEditTab(v as typeof editTab)}>
            <TabsList>
              <TabsTrigger value="basic">{t('users.tab.basic')}</TabsTrigger>
              <TabsTrigger value="access">{t('users.tab.access')}</TabsTrigger>
              <TabsTrigger value="limits">{t('users.tab.limits')}</TabsTrigger>
            </TabsList>
            <form onSubmit={handleEdit} className="space-y-4">
              {editError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{editError}</AlertDescription>
                </Alert>
              )}
              <TabsContent value="basic" className="space-y-4">
                <div className="space-y-2">
                  <Label>{t('auth.email')}</Label>
                  <Input value={editUser?.email ?? ''} disabled />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="edit-name">{t('users.displayName')}</Label>
                  <Input
                    id="edit-name"
                    value={editName}
                    onChange={(e) => setEditName(e.target.value)}
                    required
                  />
                </div>
              </TabsContent>
              <TabsContent value="access" className="space-y-4">
                <RoleAssignmentEditor
                  value={editAssignments}
                  onChange={setEditAssignments}
                  availableRoles={availableRoles}
                  availableTeams={availableTeams}
                  roleLabel={roleLabel}
                />
                <div className="space-y-2">
                  <Label>{t('users.teams')}</Label>
                  <p className="text-xs text-muted-foreground">{t('users.teamsHint')}</p>
                  <div className="flex flex-wrap gap-1">
                    {editTeamIds.map((tid) => {
                      const team = availableTeams.find((t) => t.id === tid);
                      return (
                        <Badge key={tid} variant="secondary" className="gap-1 pr-1">
                          {team?.name ?? tid}
                          <button
                            type="button"
                            className="ml-1 rounded-sm hover:bg-muted"
                            onClick={() =>
                              setEditTeamIds(editTeamIds.filter((id) => id !== tid))
                            }
                          >
                            <X className="h-3 w-3" />
                          </button>
                        </Badge>
                      );
                    })}
                  </div>
                  {availableTeams.filter((t) => !editTeamIds.includes(t.id)).length > 0 && (
                    <Select
                      onValueChange={(v) => {
                        if (v && !editTeamIds.includes(v))
                          setEditTeamIds([...editTeamIds, v]);
                      }}
                    >
                      <SelectTrigger className="w-64">
                        <SelectValue placeholder={t('users.addToTeam')} />
                      </SelectTrigger>
                      <SelectContent>
                        {availableTeams
                          .filter((t) => !editTeamIds.includes(t.id))
                          .map((t) => (
                            <SelectItem key={t.id} value={t.id}>
                              {t.name}
                            </SelectItem>
                          ))}
                      </SelectContent>
                    </Select>
                  )}
                </div>
                <EffectivePermissionsPreview
                  assignments={editAssignments}
                  availableRoles={availableRoles}
                  availablePermissions={availablePermissions}
                />
              </TabsContent>
              <TabsContent value="limits" className="pt-2">
                {editUser ? (
                  <UserLimitsTab userId={editUser.id} />
                ) : (
                  <p className="text-xs italic text-muted-foreground">
                    {t('common.loading')}
                  </p>
                )}
              </TabsContent>
              {editTab !== 'limits' && (
                <DialogFooter>
                  <Button type="submit" disabled={editLoading}>
                    {editLoading ? t('users.saving') : t('common.save')}
                  </Button>
                </DialogFooter>
              )}
            </form>
          </Tabs>
        </DialogContent>
      </Dialog>

      {/* Bulk confirm */}
      <ConfirmDialog
        open={bulkAction !== null}
        onOpenChange={(open) => {
          if (!open && !bulkBusy) setBulkAction(null);
        }}
        title={
          bulkAction === 'delete'
            ? t('users.bulk.confirmDeleteTitle')
            : bulkAction === 'deactivate'
              ? t('users.bulk.confirmDisableTitle')
              : t('users.bulk.confirmEnableTitle')
        }
        description={
          bulkAction === 'delete'
            ? t('users.bulk.confirmDeleteBody', { count: selectedUserIds.size })
            : bulkAction === 'deactivate'
              ? t('users.bulk.confirmDisableBody', { count: selectedUserIds.size })
              : t('users.bulk.confirmEnableBody', { count: selectedUserIds.size })
        }
        variant={bulkAction === 'delete' ? 'destructive' : 'default'}
        loading={bulkBusy}
        onConfirm={async () => {
          if (bulkAction) await runBulkAction(bulkAction);
        }}
      />

      {/* Confirm dialog (logout / delete / toggle) */}
      <ConfirmDialog
        open={!!confirmAction}
        onOpenChange={(open) => { if (!open) { setConfirmAction(null); setConfirmError(''); } }}
        title={confirmTitle()}
        description={confirmDesc()}
        onConfirm={handleConfirm}
        loading={confirmLoading}
        variant={confirmAction?.type === 'delete' ? 'destructive' : 'default'}
      />

      {/* Reset password confirm */}
      <ConfirmDialog
        open={!!resetConfirmUser}
        onOpenChange={(open) => { if (!open) setResetConfirmUser(null); }}
        title={t('users.resetPassword')}
        description={t('users.resetPasswordConfirm')}
        onConfirm={handleResetPassword}
        loading={resetLoading}
      />

      {/* Reset password result */}
      <Dialog open={!!resetResult} onOpenChange={(open) => { if (!open) setResetResult(null); }}>
        <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('users.resetPasswordSuccess')}</DialogTitle>
            <DialogDescription>{t('users.temporaryPasswordHint')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <Label>{t('users.temporaryPassword')}</Label>
            <div className="flex items-center gap-2">
              <Input value={resetResult?.password ?? ''} readOnly className="font-mono" />
              <Button variant="outline" size="icon" onClick={() => { navigator.clipboard.writeText(resetResult?.password ?? ''); }}>
                <Copy className="h-4 w-4" />
              </Button>
            </div>
          </div>
          <DialogFooter>
            <Button onClick={() => setResetResult(null)}>{t('common.done')}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {confirmError && (
        <Alert variant="destructive" className="fixed bottom-4 right-4 w-auto max-w-sm z-50">
          <AlertCircle className="h-4 w-4" /><AlertDescription>{confirmError}</AlertDescription>
        </Alert>
      )}
    </div>
  );
}

// ----------------------------------------------------------------------------
// Unified role assignment editor
//
// Renders every assignment (system + custom) as a removable row, and
// exposes a single picker that lists every available role. Scope is a
// structured `(scope_kind, scope_id)` twople — kind picks from a closed
// enum (`global` / `team`), id is a team UUID input that only appears
// when the kind is `team`. Result is serialized back to the
// `"global" | "team:<uuid>"` string the backend `parse_scope` helper
// accepts. The Phase-4 work below replaces the raw UUID input with a
// real team picker (Select fed by GET /api/admin/teams).
// ----------------------------------------------------------------------------

type ScopeKind = 'global' | 'team';

function parseScope(scope: string): { kind: ScopeKind; id: string } {
  if (!scope || scope === 'global') return { kind: 'global', id: '' };
  const idx = scope.indexOf(':');
  if (idx < 0) return { kind: 'global', id: '' };
  const kind = scope.slice(0, idx);
  const id = scope.slice(idx + 1);
  if (kind === 'team') return { kind, id };
  return { kind: 'global', id: '' };
}

function buildScope(kind: ScopeKind, id: string): string {
  if (kind === 'global') return 'global';
  return `${kind}:${id.trim()}`;
}

function RoleAssignmentEditor({
  value,
  onChange,
  availableRoles,
  availableTeams,
  roleLabel,
}: {
  value: RoleAssignment[];
  onChange: (next: RoleAssignment[]) => void;
  availableRoles: AvailableRole[];
  availableTeams: TeamSummary[];
  roleLabel: (name: string) => string;
}) {
  const { t } = useTranslation();
  const [pendingRoleId, setPendingRoleId] = useState('');
  const [pendingKind, setPendingKind] = useState<ScopeKind>('global');
  const [pendingScopeId, setPendingScopeId] = useState('');
  const [pendingError, setPendingError] = useState('');

  // Lookup so existing assignment rows can render team names
  // instead of raw UUIDs.
  const teamsById = new Map(availableTeams.map((t) => [t.id, t]));
  // Searchable role picker state. The popover stays open while the
  // admin types so they can refine the filter; selecting a row both
  // sets the pending role and closes it.
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerQuery, setPickerQuery] = useState('');

  // Stable lookup so the row badge can pick up `is_system` even when the
  // backend later changes a role's name (the role_id is the source of truth).
  const rolesById = new Map(availableRoles.map((r) => [r.id, r]));

  const canAdd =
    !!pendingRoleId && (pendingKind === 'global' || pendingScopeId.length > 0);

  const add = () => {
    setPendingError('');
    if (!pendingRoleId) return;
    if (pendingKind !== 'global' && !pendingScopeId) {
      setPendingError(t('users.scopeTeamRequired'));
      return;
    }
    const role = rolesById.get(pendingRoleId);
    if (!role) return;
    const scope = buildScope(pendingKind, pendingScopeId);
    if (value.some((a) => a.role_id === role.id && a.scope === scope)) return;
    onChange([
      ...value,
      { role_id: role.id, name: role.name, is_system: role.is_system, scope },
    ]);
    setPendingRoleId('');
    setPendingKind('global');
    setPendingScopeId('');
    setPickerQuery('');
  };

  const remove = (idx: number) => {
    const next = value.slice();
    next.splice(idx, 1);
    onChange(next);
  };

  return (
    <div className="space-y-2">
      <Label>{t('users.roles')}</Label>
      <p className="text-xs text-muted-foreground">{t('users.rolesDesc')}</p>
      {value.length > 0 && (
        <div className="space-y-1.5">
          {value.map((a, i) => {
            const parsed = parseScope(a.scope);
            return (
              <div
                key={`${a.role_id}-${a.scope}-${i}`}
                className="flex items-center gap-2 rounded-md border px-2 py-1.5 text-xs"
              >
                <span className="min-w-0 flex-1 truncate">
                  {a.is_system ? roleLabel(a.name) : a.name}
                </span>
                {a.is_system && (
                  <Badge variant="secondary" className="text-[10px]">
                    {t('roles.systemRole')}
                  </Badge>
                )}
                <Badge variant="outline" className="text-[10px]">
                  {parsed.kind === 'global'
                    ? t('users.scopeGlobal')
                    : `${t('users.scopeTeam')}: ${
                        teamsById.get(parsed.id)?.name ?? `${parsed.id.slice(0, 8)}…`
                      }`}
                </Badge>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6"
                  onClick={() => remove(i)}
                  aria-label={t('common.delete')}
                >
                  <Trash2 className="h-3 w-3" />
                </Button>
              </div>
            );
          })}
        </div>
      )}
      {availableRoles.length > 0 && (
        <div className="space-y-1.5">
          <div className="flex items-center gap-2">
            <div className="flex-1">
              {(() => {
                const selected = availableRoles.find((r) => r.id === pendingRoleId);
                const q = pickerQuery.trim().toLowerCase();
                // Hide rows already in `value` so the picker doesn't
                // offer the same (role, scope=global) pair twice.
                // Scope-specific dupes are still allowed because the
                // current pending kind isn't known yet.
                const assignedAtGlobal = new Set(
                  value.filter((a) => a.scope === 'global').map((a) => a.role_id),
                );
                const filtered = availableRoles.filter((r) => {
                  if (assignedAtGlobal.has(r.id)) return false;
                  if (!q) return true;
                  if (r.name.toLowerCase().includes(q)) return true;
                  if (r.is_system && roleLabel(r.name).toLowerCase().includes(q)) return true;
                  if ((r.description ?? '').toLowerCase().includes(q)) return true;
                  return false;
                });
                return (
                  <Popover open={pickerOpen} onOpenChange={setPickerOpen}>
                    <PopoverTrigger asChild>
                      <Button
                        type="button"
                        variant="outline"
                        role="combobox"
                        aria-expanded={pickerOpen}
                        className="w-full justify-between font-normal"
                      >
                        <span className={selected ? '' : 'text-muted-foreground'}>
                          {selected
                            ? selected.is_system
                              ? roleLabel(selected.name)
                              : selected.name
                            : t('users.pickRole')}
                        </span>
                        <ChevronDown className="h-4 w-4 opacity-50" />
                      </Button>
                    </PopoverTrigger>
                    <PopoverContent
                      align="start"
                      className="w-[var(--radix-popover-trigger-width)] p-0"
                    >
                      <div className="border-b p-2">
                        <Input
                          autoFocus
                          value={pickerQuery}
                          onChange={(e) => setPickerQuery(e.target.value)}
                          placeholder={t('users.pickerSearch')}
                          className="h-8"
                        />
                      </div>
                      <div className="max-h-64 overflow-y-auto py-1">
                        {filtered.length === 0 ? (
                          <p className="px-3 py-6 text-center text-xs text-muted-foreground">
                            {t('users.pickerEmpty')}
                          </p>
                        ) : (
                          filtered.map((r) => (
                            <button
                              key={r.id}
                              type="button"
                              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-muted"
                              onClick={() => {
                                setPendingRoleId(r.id);
                                setPickerOpen(false);
                                setPickerQuery('');
                              }}
                            >
                              <span
                                className={`min-w-0 flex-1 truncate ${
                                  r.is_system ? '' : 'font-mono text-xs'
                                }`}
                              >
                                {r.is_system ? roleLabel(r.name) : r.name}
                              </span>
                              {r.is_system && (
                                <Badge variant="secondary" className="text-[10px]">
                                  {t('roles.systemRole')}
                                </Badge>
                              )}
                            </button>
                          ))
                        )}
                      </div>
                    </PopoverContent>
                  </Popover>
                );
              })()}
            </div>
            <Select
              value={pendingKind}
              onValueChange={(v) => {
                setPendingKind(v as ScopeKind);
                setPendingScopeId('');
                setPendingError('');
              }}
            >
              <SelectTrigger className="w-28">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="global">{t('users.scopeGlobal')}</SelectItem>
                <SelectItem value="team">{t('users.scopeTeam')}</SelectItem>
              </SelectContent>
            </Select>
            {pendingKind !== 'global' && (
              <Select value={pendingScopeId} onValueChange={setPendingScopeId}>
                <SelectTrigger className="w-44">
                  <SelectValue placeholder={t('users.scopeTeamPick')} />
                </SelectTrigger>
                <SelectContent>
                  {availableTeams.length === 0 ? (
                    <div className="px-3 py-2 text-xs text-muted-foreground">
                      {t('users.scopeNoTeams')}
                    </div>
                  ) : (
                    availableTeams.map((team) => (
                      <SelectItem key={team.id} value={team.id}>
                        {team.name}
                      </SelectItem>
                    ))
                  )}
                </SelectContent>
              </Select>
            )}
            <Button type="button" variant="outline" size="sm" disabled={!canAdd} onClick={add}>
              <Plus className="h-3 w-3" />
            </Button>
          </div>
          {pendingError && (
            <p className="text-[10px] text-destructive">{pendingError}</p>
          )}
        </div>
      )}
    </div>
  );
}

// ----------------------------------------------------------------------------
// Effective permissions preview
//
// Shows the union of permissions / allowed_models / allowed_mcp_tools
// across the currently-selected role assignments. Computed live from
// the catalog so the admin sees what they're about to grant BEFORE
// hitting save. Mirrors the union semantics enforced server-side in
// rbac::compute_user_permissions.
//
// `null` allow_lists win — if any role grants unrestricted access,
// the union is unrestricted, matching the backend rule that "least
// privilege is expressed by NOT assigning the role".
// ----------------------------------------------------------------------------

function EffectivePermissionsPreview({
  assignments,
  availableRoles,
  availablePermissions,
}: {
  assignments: RoleAssignment[];
  availableRoles: AvailableRole[];
  availablePermissions: PermissionDef[];
}) {
  const { t } = useTranslation();

  if (assignments.length === 0) return null;

  const rolesById = new Map(availableRoles.map((r) => [r.id, r]));
  const perms = new Set<string>();
  let modelsUnrestricted = false;
  const models = new Set<string>();
  let toolsUnrestricted = false;
  const tools = new Set<string>();

  for (const a of assignments) {
    const role = rolesById.get(a.role_id);
    if (!role) continue;
    // policyToPerms needs the full catalog so it can validate literal
    // action keys AND expand wildcards like `*` or `api_keys:*` — passing
    // an empty array made every preview report zero permissions.
    const parsed = policyToPerms(JSON.stringify(role.policy_document), availablePermissions);
    for (const p of parsed.perms) perms.add(p);
    if (parsed.models === null) modelsUnrestricted = true;
    else for (const m of parsed.models) models.add(m);
    if (parsed.mcpTools === null) toolsUnrestricted = true;
    else for (const t of parsed.mcpTools) tools.add(t);
  }

  // Group permissions by their resource prefix for a compact list.
  const grouped = new Map<string, string[]>();
  for (const key of Array.from(perms).sort()) {
    const [resource, action] = key.split(':');
    const arr = grouped.get(resource) ?? [];
    arr.push(action ?? key);
    grouped.set(resource, arr);
  }

  // Collapsed by default — the panel can dump 50+ badges and was
  // overflowing the dialog on smaller screens. The summary line
  // already conveys the headline numbers; the badge grid only
  // matters when the admin wants to double-check a specific perm.
  const modelsLabel = modelsUnrestricted
    ? t('users.unrestricted')
    : `${models.size}`;
  const toolsLabel = toolsUnrestricted
    ? t('users.unrestricted')
    : `${tools.size}`;

  return (
    <Collapsible className="rounded-md border bg-muted/20 px-3 py-2">
      <CollapsibleTrigger asChild>
        <button
          type="button"
          className="group flex w-full cursor-pointer items-center gap-2 text-sm"
        >
          <ChevronRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-90" />
          <Label className="cursor-pointer font-medium">{t('users.effectivePermissions')}</Label>
          <span className="ml-auto flex items-center gap-2 text-[11px] text-muted-foreground">
            <span className="font-mono tabular-nums">{perms.size}</span>
            <span>·</span>
            <span>
              {t('users.effectiveModels')} {modelsLabel}
            </span>
            <span>·</span>
            <span>
              {t('users.effectiveTools')} {toolsLabel}
            </span>
          </span>
        </button>
      </CollapsibleTrigger>
      <CollapsibleContent className="mt-2 space-y-2">
        <p className="text-[11px] text-muted-foreground">
          {t('users.effectivePermissionsDesc')}
        </p>
        {perms.size === 0 ? (
          <p className="text-xs italic text-muted-foreground">{t('common.none')}</p>
        ) : (
          <div className="space-y-1.5">
            {Array.from(grouped.entries()).map(([resource, actions]) => (
              <div key={resource} className="flex flex-wrap items-center gap-1">
                <span className="font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
                  {resource}
                </span>
                {actions.map((a) => (
                  <Badge key={`${resource}:${a}`} variant="outline" className="text-[10px]">
                    {a}
                  </Badge>
                ))}
              </div>
            ))}
          </div>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}
