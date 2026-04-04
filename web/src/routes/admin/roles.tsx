import { useEffect, useState, useCallback, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { Shield, Plus, Pencil, Trash2 } from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';

interface SystemRole {
  name: string;
  description: string;
  permissions: string[];
}

interface CustomRole {
  id: string;
  name: string;
  description: string | null;
  is_system: boolean;
  permissions: string[];
  created_at: string;
  updated_at: string;
}

const systemRoles: SystemRole[] = [
  {
    name: 'super_admin',
    description: 'Full system access. Can manage all settings, users, providers, and view all audit logs.',
    permissions: ['users:manage', 'roles:manage', 'providers:manage', 'keys:manage', 'mcp:manage', 'analytics:view', 'audit:view', 'settings:manage'],
  },
  {
    name: 'admin',
    description: 'Administrative access. Can manage providers, keys, and MCP servers.',
    permissions: ['providers:manage', 'keys:manage', 'mcp:manage', 'analytics:view', 'audit:view', 'users:view'],
  },
  {
    name: 'team_manager',
    description: 'Team-level management. Can create API keys and view usage for their team.',
    permissions: ['keys:manage_team', 'analytics:view_team', 'mcp:view', 'providers:view'],
  },
  {
    name: 'developer',
    description: 'Standard developer access. Can use the gateway and view their own usage.',
    permissions: ['keys:view_own', 'analytics:view_own', 'mcp:view', 'providers:view', 'models:view'],
  },
  {
    name: 'viewer',
    description: 'Read-only access. Can view providers, models, and their own analytics.',
    permissions: ['analytics:view_own', 'providers:view', 'models:view'],
  },
];

export function RolesPage() {
  const { t } = useTranslation();
  const [customRoles, setCustomRoles] = useState<CustomRole[]>([]);
  const [allPermissions, setAllPermissions] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);

  // Create dialog
  const [createOpen, setCreateOpen] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formPerms, setFormPerms] = useState<Set<string>>(new Set());
  const [creating, setCreating] = useState(false);

  // Edit dialog
  const [editOpen, setEditOpen] = useState(false);
  const [editRole, setEditRole] = useState<CustomRole | null>(null);
  const [editName, setEditName] = useState('');
  const [editDesc, setEditDesc] = useState('');
  const [editPerms, setEditPerms] = useState<Set<string>>(new Set());
  const [saving, setSaving] = useState(false);

  // Delete
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [deleteId, setDeleteId] = useState<string | null>(null);

  const fetchData = useCallback(async () => {
    try {
      const [rolesRes, perms] = await Promise.all([
        api<{ items: CustomRole[] }>('/api/admin/roles'),
        api<string[]>('/api/admin/permissions'),
      ]);
      setCustomRoles(rolesRes.items);
      setAllPermissions(perms);
    } catch {
      // silently fail
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchData(); }, [fetchData]);

  const togglePerm = (set: Set<string>, setFn: (s: Set<string>) => void, perm: string) => {
    const next = new Set(set);
    if (next.has(perm)) next.delete(perm); else next.add(perm);
    setFn(next);
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setCreating(true);
    try {
      await apiPost('/api/admin/roles', { name: formName, description: formDesc || null, permissions: Array.from(formPerms) });
      setCreateOpen(false);
      setFormName(''); setFormDesc(''); setFormPerms(new Set());
      fetchData();
    } catch {
      // error
    } finally {
      setCreating(false);
    }
  };

  const openEdit = (r: CustomRole) => {
    setEditRole(r);
    setEditName(r.name);
    setEditDesc(r.description || '');
    setEditPerms(new Set(r.permissions));
    setEditOpen(true);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!editRole) return;
    setSaving(true);
    try {
      await apiPatch(`/api/admin/roles/${editRole.id}`, { name: editName, description: editDesc || null, permissions: Array.from(editPerms) });
      setEditOpen(false);
      setEditRole(null);
      fetchData();
    } catch {
      // error
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!deleteId) return;
    try {
      await apiDelete(`/api/admin/roles/${deleteId}`);
      setDeleteOpen(false);
      setDeleteId(null);
      fetchData();
    } catch {
      // error
    }
  };

  const PermissionsGrid = ({ selected, onToggle }: { selected: Set<string>; onToggle: (p: string) => void }) => (
    <div className="grid grid-cols-2 gap-1.5 max-h-48 overflow-y-auto">
      {allPermissions.map((perm) => (
        <label key={perm} className="flex items-center gap-1.5 text-xs cursor-pointer">
          <input type="checkbox" checked={selected.has(perm)} onChange={() => onToggle(perm)} className="rounded" />
          {perm}
        </label>
      ))}
    </div>
  );

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('roles.title')}</h1>
          <p className="text-muted-foreground">{t('roles.subtitle')}</p>
        </div>
        <Dialog open={createOpen} onOpenChange={setCreateOpen}>
          <DialogTrigger render={<Button onClick={() => setCreateOpen(true)}><Plus className="mr-2 h-4 w-4" />{t('roles.addRole')}</Button>} />
          <DialogContent className="max-w-md">
            <form onSubmit={handleCreate}>
              <DialogHeader>
                <DialogTitle>{t('roles.addRole')}</DialogTitle>
                <DialogDescription>{t('roles.addRoleDescription')}</DialogDescription>
              </DialogHeader>
              <div className="space-y-4 py-4">
                <div>
                  <Label>{t('common.name')}</Label>
                  <Input value={formName} onChange={(e) => setFormName(e.target.value)} required />
                </div>
                <div>
                  <Label>{t('common.description')}</Label>
                  <Input value={formDesc} onChange={(e) => setFormDesc(e.target.value)} />
                </div>
                <div>
                  <Label>{t('roles.permissions')}</Label>
                  <PermissionsGrid selected={formPerms} onToggle={(p) => togglePerm(formPerms, setFormPerms, p)} />
                </div>
              </div>
              <DialogFooter>
                <Button variant="outline" type="button" onClick={() => setCreateOpen(false)}>{t('common.cancel')}</Button>
                <Button type="submit" disabled={creating || !formName}>{creating ? t('common.loading') : t('common.create')}</Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {/* System roles */}
      <div>
        <h2 className="text-lg font-medium mb-3">{t('roles.systemRoles')}</h2>
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {systemRoles.map((role) => (
            <Card key={role.name}>
              <CardHeader className="pb-3">
                <CardTitle className="flex items-center gap-2 text-sm font-medium">
                  <Shield className="h-4 w-4 text-muted-foreground" />
                  <Badge variant="secondary">{role.name}</Badge>
                  <Badge variant="outline" className="ml-auto text-[10px]">{t('roles.systemRole')}</Badge>
                </CardTitle>
              </CardHeader>
              <CardContent className="space-y-3">
                <p className="text-xs text-muted-foreground">{role.description}</p>
                <div>
                  <p className="text-xs font-medium mb-1.5">{t('roles.permissions')}</p>
                  <div className="flex flex-wrap gap-1">
                    {role.permissions.map((perm) => (
                      <Badge key={perm} variant="outline" className="text-[10px]">{perm}</Badge>
                    ))}
                  </div>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      </div>

      <Separator />

      {/* Custom roles */}
      <div>
        <h2 className="text-lg font-medium mb-3">{t('roles.customRoles')}</h2>
        {loading ? (
          <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
        ) : customRoles.length === 0 ? (
          <p className="py-8 text-center text-sm text-muted-foreground">{t('roles.noCustomRoles')}</p>
        ) : (
          <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
            {customRoles.map((role) => (
              <Card key={role.id}>
                <CardHeader className="pb-3">
                  <CardTitle className="flex items-center gap-2 text-sm font-medium">
                    <Shield className="h-4 w-4 text-muted-foreground" />
                    <Badge variant="secondary">{role.name}</Badge>
                    <div className="ml-auto flex gap-1">
                      <Button variant="ghost" size="icon" className="h-6 w-6" onClick={() => openEdit(role)}>
                        <Pencil className="h-3 w-3" />
                      </Button>
                      <Button variant="ghost" size="icon" className="h-6 w-6 text-destructive" onClick={() => { setDeleteId(role.id); setDeleteOpen(true); }}>
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </div>
                  </CardTitle>
                </CardHeader>
                <CardContent className="space-y-3">
                  {role.description && <p className="text-xs text-muted-foreground">{role.description}</p>}
                  <div>
                    <p className="text-xs font-medium mb-1.5">{t('roles.permissions')}</p>
                    <div className="flex flex-wrap gap-1">
                      {role.permissions.map((perm) => (
                        <Badge key={perm} variant="outline" className="text-[10px]">{perm}</Badge>
                      ))}
                    </div>
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </div>

      {/* Edit dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-w-md">
          <form onSubmit={handleEdit}>
            <DialogHeader>
              <DialogTitle>{t('roles.editRole')}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div>
                <Label>{t('common.name')}</Label>
                <Input value={editName} onChange={(e) => setEditName(e.target.value)} required />
              </div>
              <div>
                <Label>{t('common.description')}</Label>
                <Input value={editDesc} onChange={(e) => setEditDesc(e.target.value)} />
              </div>
              <div>
                <Label>{t('roles.permissions')}</Label>
                <PermissionsGrid selected={editPerms} onToggle={(p) => togglePerm(editPerms, setEditPerms, p)} />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" type="button" onClick={() => setEditOpen(false)}>{t('common.cancel')}</Button>
              <Button type="submit" disabled={saving || !editName}>{saving ? t('common.loading') : t('common.save')}</Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteOpen}
        onOpenChange={(open) => { setDeleteOpen(open); if (!open) setDeleteId(null); }}
        title={t('common.delete')}
        description={t('roles.deleteConfirm')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={handleDelete}
      />
    </div>
  );
}
