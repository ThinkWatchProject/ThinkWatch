import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { getRouteApi, useNavigate } from '@tanstack/react-router';
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import {
  AlertCircle,
  ArrowLeft,
  Hash,
  Pencil,
  Plus,
  Shield,
  Trash2,
  UserPlus,
  Users,
  X,
} from 'lucide-react';
import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api';
import { fetchAllPaginated } from '@/lib/paginated-fetch';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import type { Team, TeamMember } from '@/lib/types';
import { toast } from 'sonner';

const routeApi = getRouteApi('/admin/teams/$id');

interface UserSummary {
  id: string;
  email: string;
  display_name: string;
}

export function TeamDetailPage() {
  const { t } = useTranslation();
  const { id: teamId } = routeApi.useParams();
  const navigate = useNavigate();

  const [team, setTeam] = useState<Team | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  // Members
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [membersLoading, setMembersLoading] = useState(true);
  const membersPager = useClientPagination(members, 20);

  // Edit dialog
  const [editOpen, setEditOpen] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formError, setFormError] = useState('');
  const [saving, setSaving] = useState(false);

  // Delete
  const [deleteOpen, setDeleteOpen] = useState(false);

  // Add member
  const [addMemberOpen, setAddMemberOpen] = useState(false);
  const [allUsers, setAllUsers] = useState<UserSummary[]>([]);
  const [pendingUserId, setPendingUserId] = useState('');
  const [memberError, setMemberError] = useState('');

  // Team roles
  interface TeamRole { role_id: string; name: string; is_system: boolean; assigned_at: string }
  interface AvailableRole { id: string; name: string; is_system: boolean }
  const [teamRoles, setTeamRoles] = useState<TeamRole[]>([]);
  const [rolesLoading, setRolesLoading] = useState(true);
  const [availableRoles, setAvailableRoles] = useState<AvailableRole[]>([]);
  const [assignRoleOpen, setAssignRoleOpen] = useState(false);
  const [pendingRoleId, setPendingRoleId] = useState('');

  const fetchTeam = async () => {
    try {
      const data = await api<Team>(`/api/admin/teams/${teamId}`);
      setTeam(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load team');
    } finally {
      setLoading(false);
    }
  };

  const fetchMembers = async () => {
    setMembersLoading(true);
    try {
      const data = await api<TeamMember[]>(`/api/admin/teams/${teamId}/members`);
      setMembers(data);
    } catch {
      // silently ignore — the team itself may have failed
    } finally {
      setMembersLoading(false);
    }
  };

  const fetchTeamRoles = async () => {
    setRolesLoading(true);
    try {
      const data = await api<TeamRole[]>(`/api/admin/teams/${teamId}/roles`);
      setTeamRoles(data);
    } catch {
      // silently ignore
    } finally {
      setRolesLoading(false);
    }
  };

  useEffect(() => {
    void fetchTeam();
    void fetchMembers();
    void fetchTeamRoles();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [teamId]);

  // Edit team
  const openEdit = () => {
    if (!team) return;
    setFormName(team.name);
    setFormDesc(team.description ?? '');
    setFormError('');
    setEditOpen(true);
  };

  const submitEdit = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    if (!formName.trim()) {
      setFormError(t('teams.errors.nameRequired'));
      return;
    }
    setSaving(true);
    try {
      await apiPatch(`/api/admin/teams/${teamId}`, {
        name: formName,
        description: formDesc || null,
      });
      toast.success(t('teams.toast.updated'));
      setEditOpen(false);
      await fetchTeam();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  // Add member
  const openAddMember = async () => {
    setAddMemberOpen(true);
    setPendingUserId('');
    setMemberError('');
    try {
      // Backend caps per_page at 100; loop-fetch so the picker still
      // shows everyone in orgs with 100+ users. /api/admin/users uses
      // the legacy `{data, total, per_page}` pagination shape.
      const users = await fetchAllPaginated<UserSummary>(
        '/api/admin/users',
        100,
        'data',
      ).catch(() => [] as UserSummary[]);
      setAllUsers(users);
    } catch {
      // ignore
    }
  };

  const candidateUsers = useMemo(() => {
    const inTeam = new Set(members.map((m) => m.user_id));
    return allUsers.filter((u) => !inTeam.has(u.id));
  }, [allUsers, members]);

  const addMember = async () => {
    if (!pendingUserId) return;
    setMemberError('');
    try {
      await apiPost(`/api/admin/teams/${teamId}/members`, {
        user_id: pendingUserId,
      });
      setPendingUserId('');
      setAddMemberOpen(false);
      await fetchMembers();
      await fetchTeam(); // refresh member_count
    } catch (err) {
      setMemberError(err instanceof Error ? err.message : 'Failed to add member');
    }
  };

  const removeMember = async (userId: string) => {
    try {
      await apiDelete(`/api/admin/teams/${teamId}/members/${userId}`);
      setMembers((prev) => prev.filter((m) => m.user_id !== userId));
      if (team) {
        setTeam({ ...team, member_count: Math.max(0, team.member_count - 1) });
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to remove member');
    }
  };

  if (loading) {
    return (
      <div className="space-y-6">
        <Skeleton className="h-8 w-64" />
        <div className="grid gap-4 md:grid-cols-2">
          {[...Array(2)].map((_, i) => (
            <Skeleton key={i} className="h-24 w-full" />
          ))}
        </div>
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  if (error || !team) {
    return (
      <div className="space-y-6">
        <Button variant="ghost" onClick={() => navigate({ to: '/admin/teams' })}>
          <ArrowLeft className="mr-2 h-4 w-4" />
          {t('teamDetail.backToTeams')}
        </Button>
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error || 'Team not found'}</AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-4">
          <Button
            variant="ghost"
            size="icon"
            onClick={() => navigate({ to: '/admin/teams' })}
            title={t('teamDetail.backToTeams')}
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div>
            <h1 className="text-2xl font-semibold tracking-tight">{team.name}</h1>
            {team.description && (
              <p className="text-muted-foreground">{team.description}</p>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={openEdit}
            disabled={!hasPermission('teams:update')}
          >
            <Pencil className="mr-2 h-4 w-4" />
            {t('common.edit')}
          </Button>
          <Button
            variant="destructive"
            onClick={() => setDeleteOpen(true)}
            disabled={!hasPermission('teams:delete')}
          >
            <Trash2 className="mr-2 h-4 w-4" />
            {t('common.delete')}
          </Button>
        </div>
      </div>

      {/* Stats row */}
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('teamDetail.members')}</CardTitle>
            <Users className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{team.member_count}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('common.createdAt')}</CardTitle>
            <Hash className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-sm font-medium">
              {new Date(team.created_at).toLocaleDateString()}
            </div>
          </CardContent>
        </Card>
      </div>

      {/* Tabs */}
      <Tabs defaultValue="members">
        <TabsList>
          <TabsTrigger value="members">{t('teamDetail.members')}</TabsTrigger>
          <TabsTrigger value="roles">{t('teamDetail.roles')}</TabsTrigger>
        </TabsList>

        {/* Members tab */}
        <TabsContent value="members">
          <Card>
            <CardHeader className="flex flex-row items-center justify-between">
              <CardTitle className="text-base">{t('teamDetail.members')}</CardTitle>
              {hasPermission('team_members:write') && (
                <Button size="sm" onClick={openAddMember}>
                  <UserPlus className="mr-2 h-4 w-4" />
                  {t('teamDetail.addMember')}
                </Button>
              )}
            </CardHeader>
            <CardContent>
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
                      <TableHead>{t('auth.email')}</TableHead>
                      <TableHead>{t('auth.displayName')}</TableHead>
                      <TableHead>{t('common.createdAt')}</TableHead>
                      <TableHead className="text-right">{t('common.actions')}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {membersPager.paginated.map((m) => (
                      <TableRow key={m.user_id}>
                        <TableCell className="text-sm">{m.email}</TableCell>
                        <TableCell className="text-sm">{m.display_name || '—'}</TableCell>
                        <TableCell className="text-xs text-muted-foreground">
                          {new Date(m.joined_at).toLocaleDateString()}
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
              <div className="border-t mt-4 -mx-4">
                <DataTablePagination
                  total={membersPager.total}
                  page={membersPager.page}
                  pageSize={membersPager.pageSize}
                  onPageChange={membersPager.setPage}
                  onPageSizeChange={membersPager.setPageSize}
                />
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        {/* Roles tab */}
        <TabsContent value="roles">
          <Card>
            <CardHeader className="flex flex-row items-center justify-between">
              <CardTitle className="text-base">{t('teamDetail.roles')}</CardTitle>
              {hasPermission('teams:update') && (
                <Button
                  size="sm"
                  onClick={async () => {
                    setAssignRoleOpen(true);
                    setPendingRoleId('');
                    try {
                      const data = await api<{ items: AvailableRole[] }>('/api/admin/roles');
                      setAvailableRoles(data.items ?? []);
                    } catch { /* ignore */ }
                  }}
                >
                  <Plus className="h-4 w-4 mr-1" />
                  {t('teamDetail.assignRole')}
                </Button>
              )}
            </CardHeader>
            <CardContent>
              {rolesLoading ? (
                <div className="space-y-2">
                  {[...Array(2)].map((_, i) => <Skeleton key={i} className="h-8 w-full" />)}
                </div>
              ) : teamRoles.length === 0 ? (
                <div className="flex flex-col items-center py-8 text-center">
                  <Shield className="h-8 w-8 text-muted-foreground mb-2" />
                  <p className="text-sm text-muted-foreground">{t('teamDetail.noRoles')}</p>
                  <p className="text-xs text-muted-foreground mt-1">{t('teamDetail.noRolesHint')}</p>
                </div>
              ) : (
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t('common.name')}</TableHead>
                      <TableHead>{t('teamDetail.assignedAt')}</TableHead>
                      <TableHead className="w-10" />
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {teamRoles.map((r) => (
                      <TableRow key={r.role_id}>
                        <TableCell className="font-medium">
                          {r.name}
                          {r.is_system && (
                            <Badge variant="secondary" className="ml-2 text-[10px]">{t('roles.systemRole')}</Badge>
                          )}
                        </TableCell>
                        <TableCell className="text-xs text-muted-foreground">
                          {new Date(r.assigned_at).toLocaleDateString()}
                        </TableCell>
                        <TableCell>
                          {hasPermission('teams:update') && (
                            <Button
                              variant="ghost"
                              size="icon-sm"
                              onClick={async () => {
                                try {
                                  await apiDelete(`/api/admin/teams/${teamId}/roles/${r.role_id}`);
                                  toast.success(t('teamDetail.roleRemoved'));
                                  await fetchTeamRoles();
                                } catch (err) {
                                  toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
                                }
                              }}
                            >
                              <Trash2 className="h-4 w-4" />
                            </Button>
                          )}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              )}
            </CardContent>
          </Card>
        </TabsContent>

      </Tabs>

      {/* Edit team dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitEdit}>
            <DialogHeader>
              <DialogTitle>{t('teams.editTitle')}</DialogTitle>
              <DialogDescription>{t('teams.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="edit-team-name">{t('teams.field.name')}</Label>
                <Input
                  id="edit-team-name"
                  value={formName}
                  onChange={(e) => setFormName(e.target.value)}
                  required
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="edit-team-desc">{t('teams.field.description')}</Label>
                <Textarea
                  id="edit-team-desc"
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
              <Button type="button" variant="outline" onClick={() => setEditOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={saving}>
                {saving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Add member dialog */}
      <Dialog open={addMemberOpen} onOpenChange={setAddMemberOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('teamDetail.addMember')}</DialogTitle>
            <DialogDescription>{t('teams.membersHint')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4 py-4">
            {memberError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{memberError}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label>{t('teams.addMemberUser')}</Label>
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
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setAddMemberOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button type="button" onClick={addMember} disabled={!pendingUserId}>
              <UserPlus className="mr-2 h-4 w-4" />
              {t('teamDetail.addMember')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Assign role dialog */}
      <Dialog open={assignRoleOpen} onOpenChange={setAssignRoleOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('teamDetail.assignRole')}</DialogTitle>
            <DialogDescription>{t('teamDetail.assignRoleDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4 py-4">
            <div className="space-y-2">
              <Label>{t('teamDetail.selectRole')}</Label>
              <Select value={pendingRoleId} onValueChange={setPendingRoleId}>
                <SelectTrigger>
                  <SelectValue placeholder={t('teamDetail.selectRole')} />
                </SelectTrigger>
                <SelectContent>
                  {availableRoles
                    .filter((r) => !teamRoles.some((tr) => tr.role_id === r.id))
                    .map((r) => (
                      <SelectItem key={r.id} value={r.id}>
                        {r.name}
                        {r.is_system ? ` (${t('roles.systemRole')})` : ''}
                      </SelectItem>
                    ))}
                </SelectContent>
              </Select>
            </div>
          </div>
          <DialogFooter>
            <Button
              disabled={!pendingRoleId}
              onClick={async () => {
                try {
                  await apiPost(`/api/admin/teams/${teamId}/roles`, { role_id: pendingRoleId });
                  toast.success(t('teamDetail.roleAssigned'));
                  setAssignRoleOpen(false);
                  await fetchTeamRoles();
                } catch (err) {
                  toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
                }
              }}
            >
              <Shield className="mr-2 h-4 w-4" />
              {t('teamDetail.assignRole')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={t('common.delete')}
        description={t('teams.deleteConfirm', { team: team?.name ?? '' })}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={async () => {
          try {
            await apiDelete(`/api/admin/teams/${teamId}`);
            toast.success(t('teams.toast.deleted'));
            navigate({ to: '/admin/teams' });
          } catch (err) {
            toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
          }
        }}
      />
    </div>
  );
}
