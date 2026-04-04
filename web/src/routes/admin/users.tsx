import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
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
import { Plus, MoreHorizontal, Pencil, Trash2, LogOut as LogOutIcon, KeyRound, Ban, CheckCircle, Users as UsersIcon, AlertCircle, Copy } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';

interface User {
  id: string;
  email: string;
  display_name: string;
  roles: string[];
  oidc_subject: string | null;
  is_active: boolean;
  created_at: string;
}

const ROLE_OPTIONS = ['super_admin', 'admin', 'team_manager', 'developer', 'viewer'] as const;

export function UsersPage() {
  const { t } = useTranslation();
  const [users, setUsers] = useState<User[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  // Create dialog
  const [createOpen, setCreateOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [role, setRole] = useState('developer');

  // Edit dialog
  const [editOpen, setEditOpen] = useState(false);
  const [editUser, setEditUser] = useState<User | null>(null);
  const [editName, setEditName] = useState('');
  const [editRole, setEditRole] = useState('');
  const [editError, setEditError] = useState('');
  const [editLoading, setEditLoading] = useState(false);

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
      const res = await api<{ data: User[]; total: number }>('/api/admin/users');
      setUsers(res.data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load users');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchUsers(); }, []);

  // --- Create user ---
  const resetCreateForm = () => {
    setEmail(''); setDisplayName(''); setPassword(''); setRole('developer'); setFormError('');
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError(''); setSubmitting(true);
    try {
      await apiPost('/api/admin/users', { email, display_name: displayName, password, role });
      setCreateOpen(false); resetCreateForm(); await fetchUsers();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed');
    } finally { setSubmitting(false); }
  };

  // --- Edit user ---
  const openEdit = (u: User) => {
    setEditUser(u); setEditName(u.display_name); setEditRole(u.roles[0] ?? 'developer');
    setEditError(''); setEditOpen(true);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!editUser) return;
    setEditLoading(true); setEditError('');
    try {
      await apiPatch(`/api/admin/users/${editUser.id}`, {
        display_name: editName,
        role: editRole,
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
          <DialogContent className="sm:max-w-md">
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
              <div className="space-y-2">
                <Label>{t('users.role')}</Label>
                <Select value={role} onValueChange={(v) => { if (v) setRole(v); }}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {ROLE_OPTIONS.map((r) => <SelectItem key={r} value={r}>{roleLabel(r)}</SelectItem>)}
                  </SelectContent>
                </Select>
              </div>
              <DialogFooter>
                <Button type="submit" disabled={submitting}>{submitting ? t('users.creating') : t('users.createUser')}</Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && <Alert variant="destructive"><AlertCircle className="h-4 w-4" /><AlertDescription>{error}</AlertDescription></Alert>}

      <Card>
        <CardHeader><CardTitle className="text-base">{t('users.allUsers')}</CardTitle></CardHeader>
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
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('auth.email')}</TableHead>
                  <TableHead>{t('users.displayName')}</TableHead>
                  <TableHead>{t('users.roles')}</TableHead>
                  <TableHead>{t('users.sso')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('users.created')}</TableHead>
                  <TableHead className="w-12" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {users.map((u) => (
                  <TableRow key={u.id} className={!u.is_active ? 'opacity-50' : undefined}>
                    <TableCell className="font-medium">{u.email}</TableCell>
                    <TableCell>{u.display_name || '—'}</TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {u.roles.map((r) => <Badge key={r} variant="secondary">{r}</Badge>)}
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
        <DialogContent className="sm:max-w-md">
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
            <div className="space-y-2">
              <Label>{t('users.role')}</Label>
              <Select value={editRole} onValueChange={(v) => { if (v) setEditRole(v); }}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  {ROLE_OPTIONS.map((r) => <SelectItem key={r} value={r}>{roleLabel(r)}</SelectItem>)}
                </SelectContent>
              </Select>
            </div>
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
        <DialogContent className="sm:max-w-md">
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
