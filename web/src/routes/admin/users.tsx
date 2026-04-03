import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Plus, LogOut as LogOutIcon } from 'lucide-react';
import { api, apiPost } from '@/lib/api';
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

export function UsersPage() {
  const { t } = useTranslation();
  const [users, setUsers] = useState<User[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [role, setRole] = useState('developer');

  // Force-logout confirm dialog
  const [logoutDialogOpen, setLogoutDialogOpen] = useState(false);
  const [logoutUserId, setLogoutUserId] = useState<string | null>(null);
  const [logoutLoading, setLogoutLoading] = useState(false);
  const [logoutError, setLogoutError] = useState('');

  const handleForceLogout = async () => {
    if (!logoutUserId) return;
    setLogoutLoading(true);
    setLogoutError('');
    try {
      await apiPost(`/api/admin/users/${logoutUserId}/force-logout`, {});
      setLogoutDialogOpen(false);
      setLogoutUserId(null);
    } catch (err) {
      setLogoutError(err instanceof Error ? err.message : 'Failed');
    } finally {
      setLogoutLoading(false);
    }
  };

  const fetchUsers = async () => {
    try {
      const data = await api<User[]>('/api/admin/users');
      setUsers(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load users');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchUsers(); }, []);

  const resetForm = () => {
    setEmail('');
    setDisplayName('');
    setPassword('');
    setRole('developer');
    setFormError('');
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      await apiPost('/api/admin/users', {
        email,
        display_name: displayName,
        password,
        role,
      });
      setDialogOpen(false);
      resetForm();
      await fetchUsers();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to create user');
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('users.title')}</h1>
          <p className="text-muted-foreground">{t('users.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger render={<Button />}>
            <Plus className="h-4 w-4" />
            {t('users.addUser')}
          </DialogTrigger>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>{t('users.addUser')}</DialogTitle>
              <DialogDescription>{t('users.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-4">
              {formError && (
                <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{formError}</div>
              )}
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
                <Input id="user-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder={t('auth.passwordTooShort')} required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="user-role">{t('users.role')}</Label>
                <select
                  id="user-role"
                  value={role}
                  onChange={(e) => setRole(e.target.value)}
                  className="flex h-8 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm"
                >
                  <option value="super_admin">{t('users.roleSuperAdmin', 'Super Admin')}</option>
                  <option value="admin">{t('users.roleAdmin', 'Admin')}</option>
                  <option value="team_manager">{t('users.roleTeamManager', 'Team Manager')}</option>
                  <option value="developer">{t('users.roleDeveloper', 'Developer')}</option>
                  <option value="viewer">{t('users.roleViewer', 'Viewer')}</option>
                </select>
              </div>
              <DialogFooter>
                <Button type="submit" disabled={submitting}>
                  {submitting ? t('users.creating') : t('users.createUser')}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && (
        <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{error}</div>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('users.allUsers')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('users.loadingUsers')}</p>
          ) : users.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
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
                </TableRow>
              </TableHeader>
              <TableBody>
                {users.map((u) => (
                  <TableRow key={u.id}>
                    <TableCell className="font-medium">{u.email}</TableCell>
                    <TableCell>{u.display_name || '—'}</TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {u.roles.map((r) => (
                          <Badge key={r} variant="secondary">{r}</Badge>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell>
                      {u.oidc_subject ? (
                        <Badge variant="outline">OIDC</Badge>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell>
                      <Badge variant={u.is_active ? 'default' : 'destructive'}>
                        {u.is_active ? t('common.active') : t('common.inactive')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(u.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => {
                          setLogoutUserId(u.id);
                          setLogoutDialogOpen(true);
                        }}
                      >
                        <LogOutIcon className="h-3 w-3 mr-1" />
                        {t('users.forceLogout')}
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <ConfirmDialog
        open={logoutDialogOpen}
        onOpenChange={(open) => {
          setLogoutDialogOpen(open);
          if (!open) { setLogoutUserId(null); setLogoutError(''); }
        }}
        title={t('users.forceLogout')}
        description={t('users.forceLogoutConfirm')}
        onConfirm={handleForceLogout}
        loading={logoutLoading}
      />
      {logoutError && (
        <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{logoutError}</div>
      )}
    </div>
  );
}
