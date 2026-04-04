import { useEffect, useState, useCallback, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
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
import { Checkbox } from '@/components/ui/checkbox';
import { Textarea } from '@/components/ui/textarea';
import { Shield, Plus, Pencil, Trash2, Copy, FileJson } from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { ScrollArea } from '@/components/ui/scroll-area';

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
  allowed_models: string[] | null;
  allowed_mcp_servers: string[] | null;
  policy_document: PolicyDocument | null;
  created_at: string;
  updated_at: string;
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

// --- Policy templates for quick start ---
const POLICY_TEMPLATES: Record<string, PolicyDocument> = {
  fullAccess: {
    Version: '2024-01-01',
    Statement: [{ Sid: 'FullAccess', Effect: 'Allow', Action: '*', Resource: '*' }],
  },
  readOnly: {
    Version: '2024-01-01',
    Statement: [
      { Sid: 'ReadOnly', Effect: 'Allow', Action: ['analytics:read', 'api_keys:read', 'providers:read', 'mcp_servers:read'], Resource: '*' },
      { Sid: 'DenyWrite', Effect: 'Deny', Action: '*:write', Resource: '*' },
    ],
  },
  developer: {
    Version: '2024-01-01',
    Statement: [
      { Sid: 'AllowGateway', Effect: 'Allow', Action: ['ai_gateway:use', 'mcp_gateway:use'], Resource: '*' },
      { Sid: 'AllowKeys', Effect: 'Allow', Action: ['api_keys:read', 'api_keys:write'], Resource: '*' },
      { Sid: 'AllowAnalytics', Effect: 'Allow', Action: 'analytics:read', Resource: '*' },
    ],
  },
  gatewayOnly: {
    Version: '2024-01-01',
    Statement: [
      { Sid: 'GatewayOnly', Effect: 'Allow', Action: ['ai_gateway:use', 'mcp_gateway:use'], Resource: '*' },
    ],
  },
  modelRestricted: {
    Version: '2024-01-01',
    Statement: [
      { Sid: 'AllowGateway', Effect: 'Allow', Action: 'ai_gateway:use', Resource: ['model:gpt-4o', 'model:gpt-4o-mini'] },
      { Sid: 'DenyOtherModels', Effect: 'Deny', Action: 'ai_gateway:use', Resource: 'model:*' },
    ],
  },
};

export function RolesPage() {
  const { t } = useTranslation();
  const [customRoles, setCustomRoles] = useState<CustomRole[]>([]);
  const [allPermissions, setAllPermissions] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [availableServers, setAvailableServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);

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
  const [editRole, setEditRole] = useState<CustomRole | null>(null);
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

  // Delete
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [deleteId, setDeleteId] = useState<string | null>(null);

  const fetchData = useCallback(async () => {
    try {
      const [rolesRes, perms, modelsRes, serversRes] = await Promise.all([
        api<{ items: CustomRole[] }>('/api/admin/roles'),
        api<string[]>('/api/admin/permissions'),
        api<{ data: { id: string }[] }>('/v1/models').catch(() => ({ data: [] })),
        api<{ items: McpServer[] }>('/api/mcp/servers').catch(() => ({ items: [] })),
      ]);
      setCustomRoles(rolesRes.items);
      setAllPermissions(perms);
      setAvailableModels(modelsRes.data.map((m) => m.id));
      setAvailableServers(serversRes.items);
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
    setFormPolicyError('');
    try {
      let policyDocument = null;
      if (formMode === 'policy' && formPolicyJson.trim()) {
        try {
          policyDocument = JSON.parse(formPolicyJson);
        } catch {
          setFormPolicyError(t('roles.invalidJson'));
          setCreating(false);
          return;
        }
      }
      await apiPost('/api/admin/roles', {
        name: formName,
        description: formDesc || null,
        permissions: formMode === 'simple' ? Array.from(formPerms) : [],
        allowed_models: formRestrictModels ? Array.from(formModels) : null,
        allowed_mcp_servers: formRestrictServers ? Array.from(formServers) : null,
        policy_document: policyDocument,
      });
      setCreateOpen(false);
      setFormName(''); setFormDesc(''); setFormPerms(new Set());
      setFormModels(new Set()); setFormServers(new Set());
      setFormRestrictModels(false); setFormRestrictServers(false);
      setFormPolicyJson(''); setFormMode('simple');
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
    setSaving(true);
    setEditPolicyError('');
    try {
      let policyDocument = null;
      if (editMode === 'policy' && editPolicyJson.trim()) {
        try {
          policyDocument = JSON.parse(editPolicyJson);
        } catch {
          setEditPolicyError(t('roles.invalidJson'));
          setSaving(false);
          return;
        }
      }
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
    <ScrollArea className="max-h-48">
      <div className="grid grid-cols-2 gap-1.5">
        {allPermissions.map((perm) => (
          <label key={perm} className="flex items-center gap-1.5 text-xs cursor-pointer">
            <Checkbox checked={selected.has(perm)} onCheckedChange={() => onToggle(perm)} />
            {perm}
          </label>
        ))}
      </div>
    </ScrollArea>
  );

  const ResourceConstraints = ({
    restrictModels, onRestrictModelsChange, selectedModels, onToggleModel,
    restrictServers, onRestrictServersChange, selectedServers, onToggleServer,
  }: {
    restrictModels: boolean; onRestrictModelsChange: (v: boolean) => void;
    selectedModels: Set<string>; onToggleModel: (id: string) => void;
    restrictServers: boolean; onRestrictServersChange: (v: boolean) => void;
    selectedServers: Set<string>; onToggleServer: (id: string) => void;
  }) => (
    <div className="space-y-3">
      <Label className="text-sm font-medium">{t('roles.resourceConstraints')}</Label>
      {/* Model constraints */}
      <div className="rounded-md border p-3 space-y-2">
        <label className="flex items-center gap-2 text-sm cursor-pointer">
          <Checkbox checked={restrictModels} onCheckedChange={(v) => onRestrictModelsChange(!!v)} />
          <span className="font-medium">{t('roles.allowedModels')}</span>
        </label>
        <p className="text-xs text-muted-foreground">{t('roles.allowedModelsDesc')}</p>
        {restrictModels ? (
          availableModels.length > 0 ? (
            <ScrollArea className="max-h-32">
              <div className="grid grid-cols-2 gap-1.5">
                {availableModels.map((model) => (
                  <label key={model} className="flex items-center gap-1.5 text-xs cursor-pointer">
                    <Checkbox checked={selectedModels.has(model)} onCheckedChange={() => onToggleModel(model)} />
                    <span className="truncate">{model}</span>
                  </label>
                ))}
              </div>
            </ScrollArea>
          ) : (
            <p className="text-xs text-muted-foreground italic">{t('common.noData')}</p>
          )
        ) : (
          <p className="text-xs text-muted-foreground italic">{t('roles.allModels')}</p>
        )}
      </div>
      {/* MCP server constraints */}
      <div className="rounded-md border p-3 space-y-2">
        <label className="flex items-center gap-2 text-sm cursor-pointer">
          <Checkbox checked={restrictServers} onCheckedChange={(v) => onRestrictServersChange(!!v)} />
          <span className="font-medium">{t('roles.allowedMcpServers')}</span>
        </label>
        <p className="text-xs text-muted-foreground">{t('roles.allowedMcpServersDesc')}</p>
        {restrictServers ? (
          availableServers.length > 0 ? (
            <ScrollArea className="max-h-32">
              <div className="grid grid-cols-1 gap-1.5">
                {availableServers.map((srv) => (
                  <label key={srv.id} className="flex items-center gap-1.5 text-xs cursor-pointer">
                    <Checkbox checked={selectedServers.has(srv.id)} onCheckedChange={() => onToggleServer(srv.id)} />
                    {srv.name}
                  </label>
                ))}
              </div>
            </ScrollArea>
          ) : (
            <p className="text-xs text-muted-foreground italic">{t('common.noData')}</p>
          )
        ) : (
          <p className="text-xs text-muted-foreground italic">{t('roles.allServers')}</p>
        )}
      </div>
    </div>
  );

  const RoleConstraintBadges = ({ role }: { role: CustomRole }) => (
    <>
      {role.policy_document && (
        <div>
          <p className="text-xs font-medium mb-1.5 flex items-center gap-1">
            <FileJson className="h-3 w-3" />
            {t('roles.policyMode')}
          </p>
          <div className="flex flex-wrap gap-1">
            {role.policy_document.Statement.map((stmt, i) => (
              <Badge key={i} variant={stmt.Effect === 'Allow' ? 'default' : 'destructive'} className="text-[10px]">
                {stmt.Sid ?? `${stmt.Effect} ${i}`}
              </Badge>
            ))}
          </div>
        </div>
      )}
      {role.allowed_models !== null && (
        <div>
          <p className="text-xs font-medium mb-1.5">{t('roles.allowedModels')}</p>
          <div className="flex flex-wrap gap-1">
            {role.allowed_models.length === 0 ? (
              <span className="text-[10px] text-muted-foreground italic">{t('common.none')}</span>
            ) : role.allowed_models.map((m) => (
              <Badge key={m} variant="outline" className="text-[10px] bg-blue-50 dark:bg-blue-950">{m}</Badge>
            ))}
          </div>
        </div>
      )}
      {role.allowed_mcp_servers !== null && (
        <div>
          <p className="text-xs font-medium mb-1.5">{t('roles.allowedMcpServers')}</p>
          <div className="flex flex-wrap gap-1">
            {role.allowed_mcp_servers.length === 0 ? (
              <span className="text-[10px] text-muted-foreground italic">{t('common.none')}</span>
            ) : role.allowed_mcp_servers.map((id) => {
              const srv = availableServers.find((s) => s.id === id);
              return <Badge key={id} variant="outline" className="text-[10px] bg-green-50 dark:bg-green-950">{srv?.name ?? id.slice(0, 8)}</Badge>;
            })}
          </div>
        </div>
      )}
    </>
  );

  const PolicyEditor = ({
    value, onChange, error, onApplyTemplate,
  }: {
    value: string;
    onChange: (v: string) => void;
    error: string;
    onApplyTemplate: (tpl: PolicyDocument) => void;
  }) => (
    <div className="space-y-3">
      <div>
        <Label className="text-sm font-medium">{t('roles.policyTemplates')}</Label>
        <div className="flex flex-wrap gap-1.5 mt-1.5">
          {Object.entries(POLICY_TEMPLATES).map(([key, tpl]) => (
            <Button key={key} variant="outline" size="sm" type="button"
              className="text-xs h-7"
              onClick={() => onApplyTemplate(tpl)}>
              <Copy className="mr-1 h-3 w-3" />
              {t(`roles.template_${key}`)}
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

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('roles.title')}</h1>
          <p className="text-muted-foreground">{t('roles.subtitle')}</p>
        </div>
        <Dialog open={createOpen} onOpenChange={setCreateOpen}>
          <DialogTrigger asChild><Button onClick={() => setCreateOpen(true)}><Plus className="mr-2 h-4 w-4" />{t('roles.addRole')}</Button></DialogTrigger>
          <DialogContent className="max-w-lg max-h-[90vh] overflow-y-auto">
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
                <Tabs value={formMode} onValueChange={(v) => setFormMode(v as 'simple' | 'policy')}>
                  <TabsList className="grid w-full grid-cols-2">
                    <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                    <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                  </TabsList>
                  <TabsContent value="simple" className="space-y-4 mt-3">
                    <div>
                      <Label>{t('roles.permissions')}</Label>
                      <PermissionsGrid selected={formPerms} onToggle={(p) => togglePerm(formPerms, setFormPerms, p)} />
                    </div>
                    <Separator />
                    <ResourceConstraints
                      restrictModels={formRestrictModels} onRestrictModelsChange={setFormRestrictModels}
                      selectedModels={formModels} onToggleModel={(id) => togglePerm(formModels, setFormModels, id)}
                      restrictServers={formRestrictServers} onRestrictServersChange={setFormRestrictServers}
                      selectedServers={formServers} onToggleServer={(id) => togglePerm(formServers, setFormServers, id)}
                    />
                  </TabsContent>
                  <TabsContent value="policy" className="mt-3">
                    <PolicyEditor
                      value={formPolicyJson} onChange={setFormPolicyJson} error={formPolicyError}
                      onApplyTemplate={(tpl) => setFormPolicyJson(JSON.stringify(tpl, null, 2))}
                    />
                  </TabsContent>
                </Tabs>
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
          <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
            {[...Array(3)].map((_, i) => (
              <Card key={i}>
                <CardHeader className="pb-3"><Skeleton className="h-5 w-28" /></CardHeader>
                <CardContent className="space-y-2">
                  <Skeleton className="h-3 w-full" />
                  <Skeleton className="h-3 w-2/3" />
                </CardContent>
              </Card>
            ))}
          </div>
        ) : customRoles.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-12 text-center">
            <Shield className="h-10 w-10 text-muted-foreground mb-3" />
            <p className="text-sm text-muted-foreground">{t('roles.noCustomRoles')}</p>
          </div>
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
                  <RoleConstraintBadges role={role} />
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </div>

      {/* Edit dialog */}
      <Dialog open={editOpen} onOpenChange={setEditOpen}>
        <DialogContent className="max-w-lg max-h-[90vh] overflow-y-auto">
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
              <Tabs value={editMode} onValueChange={(v) => setEditMode(v as 'simple' | 'policy')}>
                <TabsList className="grid w-full grid-cols-2">
                  <TabsTrigger value="simple">{t('roles.simpleMode')}</TabsTrigger>
                  <TabsTrigger value="policy">{t('roles.policyMode')}</TabsTrigger>
                </TabsList>
                <TabsContent value="simple" className="space-y-4 mt-3">
                  <div>
                    <Label>{t('roles.permissions')}</Label>
                    <PermissionsGrid selected={editPerms} onToggle={(p) => togglePerm(editPerms, setEditPerms, p)} />
                  </div>
                  <Separator />
                  <ResourceConstraints
                    restrictModels={editRestrictModels} onRestrictModelsChange={setEditRestrictModels}
                    selectedModels={editModels} onToggleModel={(id) => togglePerm(editModels, setEditModels, id)}
                    restrictServers={editRestrictServers} onRestrictServersChange={setEditRestrictServers}
                    selectedServers={editServers} onToggleServer={(id) => togglePerm(editServers, setEditServers, id)}
                  />
                </TabsContent>
                <TabsContent value="policy" className="mt-3">
                  <PolicyEditor
                    value={editPolicyJson} onChange={setEditPolicyJson} error={editPolicyError}
                    onApplyTemplate={(tpl) => setEditPolicyJson(JSON.stringify(tpl, null, 2))}
                  />
                </TabsContent>
              </Tabs>
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
