import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { api, apiPatch } from '@/lib/api';
import type { RoleMember, RoleResponse } from '@/routes/admin/roles/types';

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

interface RoleMembersProps {
  role: RoleResponse;
  /** Display map for `team:<id>` scopes. */
  teamsById: Map<string, { id: string; name: string }>;
  /** Bubble up changes so the caller can refetch the roles list and
   *  keep its `user_count` column in sync. */
  onMembersChanged: () => void;
}

/**
 * List + add/remove the users assigned to a role. Self-contained: owns
 * the lazy member fetch, the user-picker dropdown, and the optimistic
 * cache of `users[]` so picks survive page interactions without a
 * full refetch.
 *
 * Member writes go through PATCH /api/admin/users/{id} (replace-all
 * semantics), so we always read-modify-write the user's full
 * role_assignments to avoid clobbering unrelated assignments.
 */
export function RoleMembers({ role, teamsById, onMembersChanged }: RoleMembersProps) {
  const { t } = useTranslation();
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
      setUsers((users ?? []).map((u) => (u.id === user.id ? { ...u, role_assignments: next } : u)));
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
      const fresh = await api<{ data: PickableUser[] }>(`/api/admin/users?per_page=1000`);
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
          <Button type="button" size="sm" className="h-8" disabled={!picking || busy} onClick={addMember}>
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
      {memberError && <p className="mb-1 text-[11px] text-destructive">{memberError}</p>}
      {members === null ? (
        <div className="text-xs italic text-muted-foreground">
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
                    {(() => {
                      const teamId = m.scope.startsWith('team:') ? m.scope.slice(5) : '';
                      const team = teamsById.get(teamId);
                      return team ? `${t('users.scopeTeam')}: ${team.name}` : m.scope;
                    })()}
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
  );
}
