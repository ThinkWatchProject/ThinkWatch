import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
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
import { Plus, MoreHorizontal, Pencil, Trash2, LogOut as LogOutIcon, KeyRound, Ban, CheckCircle, Users as UsersIcon, AlertCircle, Copy, Search, ChevronRight, ChevronDown } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { LimitsPanel } from '@/components/limits/limits-panel';

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
  /// Flat permission keys this role grants. Used by the editor's
  /// "effective permissions" preview to compute the union across
  /// every assignment in real time.
  permissions: string[];
  allowed_models: string[] | null;
  allowed_mcp_servers: string[] | null;
}

interface TeamSummary {
  id: string;
  name: string;
}

export function UsersPage() {
  const { t } = useTranslation();
  const [users, setUsers] = useState<User[]>([]);
  const [availableRoles, setAvailableRoles] = useState<AvailableRole[]>([]);
  const [availableTeams, setAvailableTeams] = useState<TeamSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');

  // Create dialog
  const [createOpen, setCreateOpen] = useState(false);
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

  // Confirm dialogs
  const [confirmAction, setConfirmAction] = useState<{ type: 'logout' | 'delete' | 'toggle'; user: User } | null>(null);
  const [confirmLoading, setConfirmLoading] = useState(false);
  const [confirmError, setConfirmError] = useState('');

  // Reset password dialog
  const [resetResult, setResetResult] = useState<{ password: string; userId: string } | null>(null);
  const [resetConfirmUser, setResetConfirmUser] = useState<User | null>(null);
  const [resetLoading, setResetLoading] = useState(false);

  const fetchUsers = async () => {
    try {
      const [usersRes, rolesRes, teamsRes] = await Promise.all([
        api<{ data: User[]; total: number }>('/api/admin/users'),
        api<{ items: AvailableRole[] }>('/api/admin/roles').catch(() => ({ items: [] })),
        // Teams power the scope picker. team_managers can read this
        // endpoint too — they'll just see only their own teams,
        // which is exactly what we want for narrowing scope.
        api<TeamSummary[]>('/api/admin/teams').catch(() => []),
      ]);
      setUsers(
        usersRes.data.map((u) => ({
          ...u,
          role_assignments: u.role_assignments ?? [],
        })),
      );
      // Unified picker — system + custom roles all show up together.
      setAvailableRoles(rolesRes.items);
      setAvailableTeams(teamsRes);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load users');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchUsers(); }, []);

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

  const filteredUsers = (() => {
    const q = search.trim().toLowerCase();
    if (!q) return users;
    return users.filter((u) => {
      if (u.id.toLowerCase().includes(q)) return true;
      if (u.email.toLowerCase().includes(q)) return true;
      if (u.display_name.toLowerCase().includes(q)) return true;
      if (
        u.role_assignments.some(
          (a) =>
            a.name.toLowerCase().includes(q) ||
            roleLabel(a.name).toLowerCase().includes(q),
        )
      )
        return true;
      return false;
    });
  })();

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('users.title')}</h1>
          <p className="text-muted-foreground">{t('users.subtitle')}</p>
        </div>
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
              />
              <DialogFooter>
                <Button type="submit" disabled={submitting}>{submitting ? t('users.creating') : t('users.createUser')}</Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && <Alert variant="destructive"><AlertCircle className="h-4 w-4" /><AlertDescription>{error}</AlertDescription></Alert>}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-4 space-y-0">
          <CardTitle className="text-base">{t('users.allUsers')}</CardTitle>
          <div className="relative w-full max-w-xs">
            <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={t('users.searchPlaceholder')}
              className="pl-8"
            />
          </div>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(4)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-40" /><Skeleton className="h-4 w-24" /><Skeleton className="h-5 w-16 rounded-full" /><Skeleton className="h-5 w-14 rounded-full" /><Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : users.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <UsersIcon className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('users.noUsers')}</p>
            </div>
          ) : filteredUsers.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <UsersIcon className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('users.noMatches')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
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
                          <DropdownMenuItem onClick={() => setConfirmAction({ type: 'toggle', user: u })}>
                            {u.is_active
                              ? <><Ban className="h-4 w-4 mr-2" />{t('users.deactivate')}</>
                              : <><CheckCircle className="h-4 w-4 mr-2" />{t('users.activate')}</>}
                          </DropdownMenuItem>
                          <DropdownMenuItem onClick={() => setConfirmAction({ type: 'logout', user: u })}>
                            <LogOutIcon className="h-4 w-4 mr-2" />{t('users.forceLogout')}
                          </DropdownMenuItem>
                          <DropdownMenuSeparator />
                          <DropdownMenuItem className="text-destructive" onClick={() => setConfirmAction({ type: 'delete', user: u })}>
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
      </Card>

      {/* Edit dialog */}
      <Dialog open={editOpen} onOpenChange={(open) => { setEditOpen(open); if (!open) setEditUser(null); }}>
        <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('users.editUser')}</DialogTitle>
            <DialogDescription>{t('users.editDescription')}</DialogDescription>
          </DialogHeader>
          <form onSubmit={handleEdit} className="space-y-4">
            {editError && <Alert variant="destructive"><AlertCircle className="h-4 w-4" /><AlertDescription>{editError}</AlertDescription></Alert>}
            <div className="space-y-2">
              <Label>{t('auth.email')}</Label>
              <Input value={editUser?.email ?? ''} disabled />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-name">{t('users.displayName')}</Label>
              <Input id="edit-name" value={editName} onChange={(e) => setEditName(e.target.value)} required />
            </div>
            <RoleAssignmentEditor
              value={editAssignments}
              onChange={setEditAssignments}
              availableRoles={availableRoles}
              availableTeams={availableTeams}
              roleLabel={roleLabel}
            />
            <EffectivePermissionsPreview
              assignments={editAssignments}
              availableRoles={availableRoles}
            />
            {editUser && (
              <LimitsPanel
                subjectKind="user"
                subjectId={editUser.id}
                surfaces={['ai_gateway', 'mcp_gateway']}
                allowBudgets={true}
              />
            )}
            <DialogFooter>
              <Button type="submit" disabled={editLoading}>{editLoading ? t('users.saving') : t('common.save')}</Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

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
// Shows the union of permissions / allowed_models / allowed_mcp_servers
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
}: {
  assignments: RoleAssignment[];
  availableRoles: AvailableRole[];
}) {
  const { t } = useTranslation();

  if (assignments.length === 0) return null;

  const rolesById = new Map(availableRoles.map((r) => [r.id, r]));
  const perms = new Set<string>();
  let modelsUnrestricted = false;
  const models = new Set<string>();
  let serversUnrestricted = false;
  const servers = new Set<string>();

  for (const a of assignments) {
    const role = rolesById.get(a.role_id);
    if (!role) continue;
    for (const p of role.permissions) perms.add(p);
    if (role.allowed_models === null) modelsUnrestricted = true;
    else for (const m of role.allowed_models) models.add(m);
    if (role.allowed_mcp_servers === null) serversUnrestricted = true;
    else for (const s of role.allowed_mcp_servers) servers.add(s);
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
  const serversLabel = serversUnrestricted
    ? t('users.unrestricted')
    : `${servers.size}`;

  return (
    <details className="rounded-md border bg-muted/20 px-3 py-2 [&[open]>summary>svg]:rotate-90">
      <summary className="flex cursor-pointer items-center gap-2 text-sm">
        <ChevronRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform" />
        <Label className="cursor-pointer font-medium">
          {t('users.effectivePermissions')}
        </Label>
        <span className="ml-auto flex items-center gap-2 text-[11px] text-muted-foreground">
          <span className="font-mono tabular-nums">{perms.size}</span>
          <span>·</span>
          <span>
            {t('users.effectiveModels')} {modelsLabel}
          </span>
          <span>·</span>
          <span>
            {t('users.effectiveServers')} {serversLabel}
          </span>
        </span>
      </summary>
      <div className="mt-2 space-y-2">
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
      </div>
    </details>
  );
}
