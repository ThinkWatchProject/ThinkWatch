import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Plus, Ban, RotateCw, Pencil, KeyRound, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, hasPermission } from '@/lib/api';
import { fetchAllPaginated } from '@/lib/paginated-fetch';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';
import {
  CreateApiKeyDialog,
  EditApiKeyDialog,
  RotateApiKeyDialog,
  DeleteApiKeyDialog,
  type ApiKey,
} from './api-key-dialogs';
import type {
  ModelsByProvider,
  McpToolsByServer,
} from '@/components/roles/PermissionTree';

interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
}

interface McpToolRow {
  id: string;
  server_name: string;
  name: string;
  namespaced_name: string;
}

interface PolicyScope {
  allowed_models: string[] | null;
  allowed_mcp_tools: string[] | null;
}

/// True when `id` is covered by the caller's role-granted `allowed` list.
/// Mirrors the gateway's `request.model == g || request.model.starts_with(g)`
/// so the picker only surfaces models the eventual request would accept.
function modelAllowed(id: string, allowed: string[] | null): boolean {
  if (allowed === null) return true;
  return allowed.some((g) => id === g || id.startsWith(g));
}

/// Mirrors the MCP access-control pattern grammar from
/// `crates/mcp-gateway/src/access_control.rs`: `*`, `<server>__*`, or
/// `<server>__<tool>`.
function mcpToolAllowed(key: string, allowed: string[] | null): boolean {
  if (allowed === null) return true;
  return allowed.some((p) => {
    if (p === '*' || p === key) return true;
    if (p.endsWith('__*')) {
      const prefix = p.slice(0, -'__*'.length);
      return key.startsWith(prefix + '__');
    }
    return false;
  });
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface PaginatedResponse<T> {
  data: T[];
  total: number;
  page: number;
  per_page: number;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function daysUntilExpiry(expiresAt: string | null): number | null {
  if (!expiresAt) return null;
  const diff = new Date(expiresAt).getTime() - Date.now();
  return diff / (1000 * 60 * 60 * 24);
}

function StatusBadge({ apiKey }: { apiKey: ApiKey }) {
  const { t } = useTranslation();

  if (apiKey.disabled_reason) {
    const labelMap: Record<string, string> = {
      expired: t('apiKeys.expired'),
      inactive: t('common.inactive'),
      rotated: t('apiKeys.rotated'),
      revoked: t('apiKeys.revoked'),
    };
    return (
      <Badge variant="destructive">
        {labelMap[apiKey.disabled_reason] ?? apiKey.disabled_reason}
      </Badge>
    );
  }

  if (!apiKey.is_active) {
    return <Badge variant="destructive">{t('apiKeys.revoked')}</Badge>;
  }

  return <Badge variant="default">{t('common.active')}</Badge>;
}

function ExpiryCell({ apiKey }: { apiKey: ApiKey }) {
  const { t } = useTranslation();
  const effectiveExpiry =
    apiKey.expires_at && apiKey.grace_period_ends_at
      ? new Date(apiKey.expires_at) < new Date(apiKey.grace_period_ends_at)
        ? apiKey.expires_at
        : apiKey.grace_period_ends_at
      : apiKey.expires_at ?? apiKey.grace_period_ends_at;

  if (!effectiveExpiry) return <span>{t('apiKeys.never')}</span>;

  const days = daysUntilExpiry(effectiveExpiry);
  const dateStr = new Date(effectiveExpiry).toLocaleDateString();
  const inGrace = !!apiKey.grace_period_ends_at && effectiveExpiry === apiKey.grace_period_ends_at;

  const graceTag = inGrace ? (
    <Badge className="bg-amber-500/15 text-amber-700 dark:text-amber-400 border-amber-500/30 text-[10px] px-1 py-0">
      {t('apiKeys.gracePeriod')}
    </Badge>
  ) : null;

  if (days !== null && days < 0) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        {graceTag}
        <Badge variant="destructive" className="text-[10px] px-1 py-0">{t('apiKeys.expired')}</Badge>
      </span>
    );
  }

  if (days !== null && days < 1) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        {graceTag}
        <Badge variant="destructive" className="text-[10px] px-1 py-0">&lt;1d</Badge>
      </span>
    );
  }

  if (days !== null && days < 7) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        {graceTag}
        <Badge className="bg-yellow-500/15 text-yellow-700 dark:text-yellow-400 border-yellow-500/30 text-[10px] px-1 py-0">
          {Math.ceil(days)}d
        </Badge>
      </span>
    );
  }

  return (
    <span className="flex items-center gap-1.5">
      {dateStr}
      {graceTag}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function ApiKeysPage() {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [tab, setTab] = useState('all');

  // Shared data for dialogs
  const [costCenterOptions, setCostCenterOptions] = useState<string[]>([]);
  // Full catalog — filtered down to the caller's policy scope below
  // before it reaches the dialog pickers.
  const [availableModels, setAvailableModels] = useState<ModelRow[]>([]);
  const [availableMcpTools, setAvailableMcpTools] = useState<McpToolRow[]>([]);
  const [policyScope, setPolicyScope] = useState<PolicyScope>({
    allowed_models: null,
    allowed_mcp_tools: null,
  });

  // Dialog states
  const [createDialogOpen, setCreateDialogOpen] = useState(false);
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editingKey, setEditingKey] = useState<ApiKey | null>(null);
  const [rotateDialogOpen, setRotateDialogOpen] = useState(false);
  const [rotatingKey, setRotatingKey] = useState<ApiKey | null>(null);
  const [revokeTargetId, setRevokeTargetId] = useState<string | null>(null);

  // ---------------------------------------------------------------------------
  // Data fetching
  // ---------------------------------------------------------------------------

  // The "Revoked" tab needs the archived view from the server — those
  // rows have `deleted_at IS NOT NULL` and the default list endpoint
  // hides them. Other tabs filter client-side off the live result set.
  const fetchKeys = async (mode: 'live' | 'archived' = 'live') => {
    try {
      const url =
        mode === 'archived' ? '/api/keys?archived=true' : '/api/keys';
      const res = await api<PaginatedResponse<ApiKey>>(url);
      setKeys(res.data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load API keys');
    } finally {
      setLoading(false);
    }
  };

  // Keys re-fetch on tab change because the "Revoked" tab pulls from
  // the archived view (different server-side filter), not the same
  // result set with a client-side mask.
  useEffect(() => {
    setLoading(true);
    fetchKeys(tab === 'revoked' ? 'archived' : 'live');
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  useEffect(() => {
    api<string[]>('/api/keys/cost-centers')
      .then(setCostCenterOptions)
      .catch(() => setCostCenterOptions([]));
    // /api/admin/models and /api/mcp/tools are both paginated and clamp
    // page_size to 200; the dialog pickers want the complete catalog so
    // they can be filtered down to the caller's policy scope locally.
    fetchAllPaginated<ModelRow>('/api/admin/models')
      .then(setAvailableModels)
      .catch(() => setAvailableModels([]));
    fetchAllPaginated<McpToolRow>('/api/mcp/tools')
      .then(setAvailableMcpTools)
      .catch(() => setAvailableMcpTools([]));
    api<PolicyScope>('/api/keys/policy-scope')
      .then(setPolicyScope)
      .catch(() =>
        setPolicyScope({ allowed_models: null, allowed_mcp_tools: null }),
      );
  }, []);

  // Shape + filter the raw catalogs into the maps the scope dropdowns
  // consume. `allowed_models === null` means the caller's roles grant
  // everything, so the picker shows the full catalog.
  const modelsByProvider = useMemo<ModelsByProvider>(() => {
    const bucket = availableModels
      .filter((m) => modelAllowed(m.model_id, policyScope.allowed_models))
      .map((m) => ({ modelId: m.model_id, displayName: m.display_name }));
    const out = new Map<string, { modelId: string; displayName: string }[]>();
    if (bucket.length > 0) out.set('', bucket);
    return out;
  }, [availableModels, policyScope.allowed_models]);

  const mcpToolsByServer = useMemo<McpToolsByServer>(() => {
    const out: McpToolsByServer = new Map();
    for (const tool of availableMcpTools) {
      if (!mcpToolAllowed(tool.namespaced_name, policyScope.allowed_mcp_tools)) {
        continue;
      }
      // namespaced_name is `<prefix>__<tool_name>` (tool_name itself may
      // contain single underscores but never `__`).
      const sep = tool.namespaced_name.indexOf('__');
      const prefix = sep >= 0 ? tool.namespaced_name.slice(0, sep) : '';
      const entry = out.get(tool.server_name) ?? {
        serverName: tool.server_name,
        prefix,
        tools: [] as { key: string; toolName: string }[],
      };
      entry.tools.push({ key: tool.namespaced_name, toolName: tool.name });
      out.set(tool.server_name, entry);
    }
    return out;
  }, [availableMcpTools, policyScope.allowed_mcp_tools]);

  // ---------------------------------------------------------------------------
  // Filtered keys
  // ---------------------------------------------------------------------------

  const filteredKeys =
    tab === 'expiring'
      ? keys.filter((k) => {
          if (!k.is_active || !k.expires_at) return false;
          const days = daysUntilExpiry(k.expires_at);
          return days !== null && days >= 0 && days < 7;
        })
      : keys;

  // ---------------------------------------------------------------------------
  // Callbacks
  // ---------------------------------------------------------------------------

  const handleCostCenterAdded = (tag: string) => {
    if (!costCenterOptions.includes(tag)) {
      setCostCenterOptions((prev) => [...prev, tag].sort());
    }
  };

  const openEditDialog = (k: ApiKey) => {
    setEditingKey(k);
    setEditDialogOpen(true);
  };

  const openRotateDialog = (k: ApiKey) => {
    setRotatingKey(k);
    setRotateDialogOpen(true);
  };

  const handleRevokeSuccess = () => {
    setRevokeTargetId(null);
    toast.success(t('common.deleteSuccess'));
    fetchKeys();
  };

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('apiKeys.title')}</h1>
          <p className="text-muted-foreground">{t('apiKeys.subtitle')}</p>
        </div>
        {hasPermission('api_keys:create') && (
          <Button onClick={() => setCreateDialogOpen(true)}>
            <Plus className="h-4 w-4" />
            {t('apiKeys.createKey')}
          </Button>
        )}
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {/* ---- Dialogs ---- */}
      <CreateApiKeyDialog
        open={createDialogOpen}
        onOpenChange={setCreateDialogOpen}
        onSuccess={fetchKeys}
        modelsByProvider={modelsByProvider}
        mcpToolsByServer={mcpToolsByServer}
        costCenterOptions={costCenterOptions}
        onCostCenterAdded={handleCostCenterAdded}
      />

      <EditApiKeyDialog
        open={editDialogOpen}
        onOpenChange={setEditDialogOpen}
        onSuccess={fetchKeys}
        apiKey={editingKey}
        modelsByProvider={modelsByProvider}
        mcpToolsByServer={mcpToolsByServer}
        costCenterOptions={costCenterOptions}
        onCostCenterAdded={handleCostCenterAdded}
      />

      <RotateApiKeyDialog
        open={rotateDialogOpen}
        onOpenChange={setRotateDialogOpen}
        onSuccess={fetchKeys}
        apiKey={rotatingKey}
      />

      <DeleteApiKeyDialog
        open={revokeTargetId !== null}
        onOpenChange={(open) => { if (!open) setRevokeTargetId(null); }}
        onSuccess={handleRevokeSuccess}
        keyId={revokeTargetId}
      />

      {/* ---- Main content ---- */}
      <div className="mb-4 flex items-center">
        <Tabs value={tab} onValueChange={setTab}>
          <TabsList>
            <TabsTrigger value="all">{t('common.total')}</TabsTrigger>
            <TabsTrigger value="expiring">{t('apiKeys.expiringSoon')}</TabsTrigger>
            <TabsTrigger value="revoked">{t('apiKeys.revoked')}</TabsTrigger>
          </TabsList>
        </Tabs>
      </div>

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-3 p-4">
              {[...Array(3)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-28" />
                  <Skeleton className="h-4 w-20 font-mono" />
                  <Skeleton className="h-4 w-16" />
                  <Skeleton className="h-4 w-12" />
                  <Skeleton className="h-4 w-20" />
                  <Skeleton className="h-5 w-14 rounded-full" />
                  <Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : filteredKeys.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center text-center">
              <KeyRound className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">
                {tab === 'all' ? t('apiKeys.noKeys') : t('common.noData')}
              </p>
              {tab === 'all' && (
                <>
                  <p className="text-xs text-muted-foreground mt-1">{t('apiKeys.noKeysHint')}</p>
                  {hasPermission('api_keys:create') && (
                    <Button
                      onClick={() => setCreateDialogOpen(true)}
                      size="sm"
                      className="mt-4"
                    >
                      <Plus className="h-4 w-4" />
                      {t('apiKeys.createKey')}
                    </Button>
                  )}
                </>
              )}
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('apiKeys.keyPrefix')}</TableHead>
                  <TableHead className="hidden md:table-cell">{t('apiKeys.surfaces')}</TableHead>
                  <TableHead className="hidden lg:table-cell">{t('apiKeys.expires')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead className="hidden lg:table-cell">{t('common.createdAt')}</TableHead>
                  <TableHead className="w-28" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {filteredKeys.map((k) => (
                  <TableRow key={k.id}>
                    <TableCell className="font-medium">{k.name}</TableCell>
                    <TableCell>
                      <code className="rounded bg-muted px-1.5 py-0.5 text-xs">{k.key_prefix}</code>
                    </TableCell>
                    <TableCell className="hidden md:table-cell">
                      <div className="flex flex-wrap gap-1">
                        {(k.surfaces ?? []).map((s) => (
                          <Badge key={s} variant="outline" className="text-[10px]">
                            {t(`apiKeys.surfaceShort_${s}` as const)}
                          </Badge>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell className="hidden lg:table-cell text-xs text-muted-foreground">
                      <ExpiryCell apiKey={k} />
                    </TableCell>
                    <TableCell>
                      <StatusBadge apiKey={k} />
                    </TableCell>
                    <TableCell className="hidden lg:table-cell text-xs text-muted-foreground">
                      {new Date(k.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => openEditDialog(k)}
                          title={t('common.edit')}
                          aria-label={t('common.edit')}
                        >
                          <Pencil className="h-4 w-4" />
                        </Button>
                        {k.is_active && k.disabled_reason !== 'rotated' && (
                          <Button
                            variant="ghost"
                            size="icon-sm"
                            onClick={() => openRotateDialog(k)}
                            title={t('apiKeys.rotate')}
                            aria-label={t('apiKeys.rotate')}
                          >
                            <RotateCw className="h-4 w-4" />
                          </Button>
                        )}
                        {k.is_active && (
                          <Button
                            variant="ghost"
                            size="icon-sm"
                            onClick={() => setRevokeTargetId(k.id)}
                            title={t('common.revoke')}
                            aria-label={t('common.revoke')}
                          >
                            <Ban className="h-4 w-4" />
                          </Button>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
