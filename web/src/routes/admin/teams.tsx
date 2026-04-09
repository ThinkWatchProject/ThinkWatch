import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { AlertCircle, Pencil, Plus, Trash2, UserPlus, Users, X } from 'lucide-react';
import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api';
import { toast } from 'sonner';

interface Team {
  id: string;
  name: string;
  description: string | null;
  member_count: number;
  created_at: string;
}

interface TeamMember {
  user_id: string;
  email: string;
  display_name: string;
  role: string;
  joined_at: string;
}

interface UserSummary {
  id: string;
  email: string;
  display_name: string;
}

export function TeamsPage() {
  const { t } = useTranslation();
  const [teams, setTeams] = useState<Team[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  // Create / edit
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<Team | null>(null);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formError, setFormError] = useState('');
  const [saving, setSaving] = useState(false);

  // Delete
  const [deleteTarget, setDeleteTarget] = useState<Team | null>(null);

  // Members
  const [membersOpen, setMembersOpen] = useState<Team | null>(null);
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [membersLoading, setMembersLoading] = useState(false);
  const [allUsers, setAllUsers] = useState<UserSummary[]>([]);
  const [pendingUserId, setPendingUserId] = useState('');
  const [pendingRole, setPendingRole] = useState<'member' | 'manager'>('member');
  const [memberError, setMemberError] = useState('');

  const fetchTeams = async () => {
    setLoading(true);
    try {
      const data = await api<Team[]>('/api/admin/teams');
      setTeams(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load teams');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void fetchTeams();
  }, []);

  // ----- Create / edit -----

  const openCreate = () => {
    setEditing(null);
    setFormName('');
    setFormDesc('');
    setFormError('');
    setDialogOpen(true);
  };

  const openEdit = (team: Team) => {
    setEditing(team);
    setFormName(team.name);
    setFormDesc(team.description ?? '');
    setFormError('');
    setDialogOpen(true);
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    if (!formName.trim()) {
      setFormError(t('teams.errors.nameRequired'));
      return;
    }
    setSaving(true);
    try {
      if (editing) {
        await apiPatch(`/api/admin/teams/${editing.id}`, {
          name: formName,
          description: formDesc || null,
        });
        toast.success(t('teams.toast.updated'));
      } else {
        await apiPost('/api/admin/teams', { name: formName, description: formDesc || null });
        toast.success(t('teams.toast.created'));
      }
      setDialogOpen(false);
      await fetchTeams();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    try {
      await apiDelete(`/api/admin/teams/${deleteTarget.id}`);
      toast.success(t('teams.toast.deleted'));
      setDeleteTarget(null);
      await fetchTeams();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
    }
  };

  // ----- Members -----

  const openMembers = async (team: Team) => {
    setMembersOpen(team);
    setMembers([]);
    setMembersLoading(true);
    setMemberError('');
    setPendingUserId('');
    setPendingRole('member');
    try {
      const [m, users] = await Promise.all([
        api<TeamMember[]>(`/api/admin/teams/${team.id}/members`),
        // We need a way to look up candidate users to add. The
        // existing /api/admin/users endpoint is gated by users:read
        // — admins have it. team_managers will see only their own
        // owned set, which is fine: they're typically adding
        // existing employees who landed here via SSO or were created
        // by a super_admin.
        api<{ data: UserSummary[] }>('/api/admin/users?per_page=200').catch(() => ({ data: [] })),
      ]);
      setMembers(m);
      setAllUsers(users.data ?? []);
    } catch (err) {
      setMemberError(err instanceof Error ? err.message : 'Failed to load members');
    } finally {
      setMembersLoading(false);
    }
  };

  const addMember = async () => {
    if (!membersOpen || !pendingUserId) return;
    setMemberError('');
    try {
      await apiPost(`/api/admin/teams/${membersOpen.id}/members`, {
        user_id: pendingUserId,
        role: pendingRole,
      });
      setPendingUserId('');
      // Refresh the members list
      const m = await api<TeamMember[]>(`/api/admin/teams/${membersOpen.id}/members`);
      setMembers(m);
      // Bump the count locally so the table updates without a full fetch
      setTeams((prev) =>
        prev.map((t) =>
          t.id === membersOpen.id ? { ...t, member_count: t.member_count + 1 } : t,
        ),
      );
    } catch (err) {
      setMemberError(err instanceof Error ? err.message : 'Failed to add member');
    }
  };

  const removeMember = async (userId: string) => {
    if (!membersOpen) return;
    try {
      await apiDelete(`/api/admin/teams/${membersOpen.id}/members/${userId}`);
      setMembers((prev) => prev.filter((m) => m.user_id !== userId));
      setTeams((prev) =>
        prev.map((t) =>
          t.id === membersOpen.id ? { ...t, member_count: Math.max(0, t.member_count - 1) } : t,
        ),
      );
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to remove member');
    }
  };

  // Users not already in the team — for the add picker.
  const candidateUsers = useMemo(() => {
    const inTeam = new Set(members.map((m) => m.user_id));
    return allUsers.filter((u) => !inTeam.has(u.id));
  }, [allUsers, members]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('teams.title')}</h1>
          <p className="text-muted-foreground">{t('teams.subtitle')}</p>
        </div>
        <Button onClick={openCreate} disabled={!hasPermission('teams:create')}>
          <Plus className="mr-2 h-4 w-4" />
          {t('teams.addTeam')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {loading ? (
        <div className="space-y-4">
          {[...Array(3)].map((_, i) => (
            <Skeleton key={i} className="h-16 w-full" />
          ))}
        </div>
      ) : teams.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Users className="mb-3 h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('teams.noTeams')}</p>
            <p className="mt-1 text-xs text-muted-foreground">{t('teams.noTeamsHint')}</p>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('teams.allTeams')}</CardTitle>
          </CardHeader>
          <CardContent>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('teams.col.name')}</TableHead>
                  <TableHead>{t('teams.col.description')}</TableHead>
                  <TableHead className="text-center">{t('teams.col.members')}</TableHead>
                  <TableHead className="text-right">{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {teams.map((team) => (
                  <TableRow key={team.id}>
                    <TableCell className="font-medium">{team.name}</TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {team.description || '—'}
                    </TableCell>
                    <TableCell className="text-center">
                      <Badge variant="secondary">{team.member_count}</Badge>
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => openMembers(team)}
                        title={t('teams.manageMembers')}
                      >
                        <Users className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => openEdit(team)}
                        title={t('common.edit')}
                        disabled={!hasPermission('teams:update')}
                      >
                        <Pencil className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => setDeleteTarget(team)}
                        title={t('common.delete')}
                        disabled={!hasPermission('teams:delete')}
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent>
        </Card>
      )}

      {/* --- Create / edit dialog --- */}
      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submit}>
            <DialogHeader>
              <DialogTitle>
                {editing ? t('teams.editTitle') : t('teams.createTitle')}
              </DialogTitle>
              <DialogDescription>{t('teams.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="team-name">{t('teams.field.name')}</Label>
                <Input
                  id="team-name"
                  value={formName}
                  onChange={(e) => setFormName(e.target.value)}
                  placeholder="engineering"
                  required
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="team-desc">{t('teams.field.description')}</Label>
                <Textarea
                  id="team-desc"
                  value={formDesc}
                  onChange={(e) => setFormDesc(e.target.value)}
                  rows={3}
                />
              </div>
              {formError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{formError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setDialogOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={saving}>
                {saving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* --- Members dialog --- */}
      <Dialog open={membersOpen !== null} onOpenChange={(o) => !o && setMembersOpen(null)}>
        <DialogContent className="sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              {t('teams.membersTitle', { team: membersOpen?.name ?? '' })}
            </DialogTitle>
            <DialogDescription>{t('teams.membersHint')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            {memberError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{memberError}</AlertDescription>
              </Alert>
            )}

            {hasPermission('team_members:write') && (
              <div className="flex items-end gap-2">
                <div className="flex-1 space-y-1">
                  <Label className="text-xs">{t('teams.addMemberUser')}</Label>
                  <Select value={pendingUserId} onValueChange={setPendingUserId}>
                    <SelectTrigger>
                      <SelectValue placeholder={t('teams.selectUser')} />
                    </SelectTrigger>
                    <SelectContent>
                      {candidateUsers.map((u) => (
                        <SelectItem key={u.id} value={u.id}>
                          {u.display_name || u.email}{' '}
                          <span className="text-muted-foreground">({u.email})</span>
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-1">
                  <Label className="text-xs">{t('teams.role')}</Label>
                  <Select
                    value={pendingRole}
                    onValueChange={(v) => setPendingRole(v as 'member' | 'manager')}
                  >
                    <SelectTrigger className="w-32">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="member">{t('teams.roleMember')}</SelectItem>
                      <SelectItem value="manager">{t('teams.roleManager')}</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <Button
                  type="button"
                  size="sm"
                  onClick={addMember}
                  disabled={!pendingUserId}
                >
                  <UserPlus className="mr-1 h-4 w-4" />
                  {t('teams.add')}
                </Button>
              </div>
            )}

            {membersLoading ? (
              <div className="space-y-2">
                {[...Array(3)].map((_, i) => (
                  <Skeleton key={i} className="h-10 w-full" />
                ))}
              </div>
            ) : members.length === 0 ? (
              <p className="py-8 text-center text-sm text-muted-foreground">
                {t('teams.noMembers')}
              </p>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('teams.member')}</TableHead>
                    <TableHead>{t('teams.role')}</TableHead>
                    <TableHead className="text-right">{t('common.actions')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {members.map((m) => (
                    <TableRow key={m.user_id}>
                      <TableCell>
                        <div className="font-medium">{m.display_name || m.email}</div>
                        <div className="text-xs text-muted-foreground">{m.email}</div>
                      </TableCell>
                      <TableCell>
                        <Badge variant="outline">{m.role}</Badge>
                      </TableCell>
                      <TableCell className="text-right">
                        <Button
                          variant="ghost"
                          size="icon"
                          onClick={() => removeMember(m.user_id)}
                          disabled={!hasPermission('team_members:write')}
                          title={t('teams.remove')}
                        >
                          <X className="h-4 w-4" />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setMembersOpen(null)}>
              {t('common.done')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(o) => !o && setDeleteTarget(null)}
        title={t('teams.deleteTitle')}
        description={t('teams.deleteConfirm', { team: deleteTarget?.name ?? '' })}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDelete}
      />
    </div>
  );
}
