import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
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
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';
import {
  CreateApiKeyDialog,
  EditApiKeyDialog,
  RotateApiKeyDialog,
  DeleteApiKeyDialog,
  type ApiKey,
  type ModelRow,
} from './api-key-dialogs';

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
  const [availableModels, setAvailableModels] = useState<ModelRow[]>([]);

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

  const fetchKeys = async () => {
    try {
      const res = await api<PaginatedResponse<ApiKey>>('/api/keys');
      setKeys(res.data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load API keys');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchKeys();
    api<string[]>('/api/keys/cost-centers')
      .then(setCostCenterOptions)
      .catch(() => setCostCenterOptions([]));
    // /api/admin/models returns `{ items, total }`, not a bare array.
    // Request a wide page so the allowed-models picker sees every row.
    api<{ items: ModelRow[]; total: number }>('/api/admin/models?page_size=200')
      .then((res) => setAvailableModels(res.items))
      .catch(() => setAvailableModels([]));
  }, []);

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
    <div className="space-y-6">
      <div className="flex items-center justify-between">
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
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {/* ---- Dialogs ---- */}
      <CreateApiKeyDialog
        open={createDialogOpen}
        onOpenChange={setCreateDialogOpen}
        onSuccess={fetchKeys}
        availableModels={availableModels}
        costCenterOptions={costCenterOptions}
        onCostCenterAdded={handleCostCenterAdded}
      />

      <EditApiKeyDialog
        open={editDialogOpen}
        onOpenChange={setEditDialogOpen}
        onSuccess={fetchKeys}
        apiKey={editingKey}
        availableModels={availableModels}
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
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <CardTitle className="text-base">{t('apiKeys.allKeys')}</CardTitle>
            <Tabs value={tab} onValueChange={setTab}>
              <TabsList>
                <TabsTrigger value="all">{t('common.total')}</TabsTrigger>
                <TabsTrigger value="expiring">{t('apiKeys.expiringSoon')}</TabsTrigger>
              </TabsList>
            </Tabs>
          </div>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
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
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <KeyRound className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">
                {tab === 'expiring' ? t('common.noData') : t('apiKeys.noKeys')}
              </p>
              {tab === 'all' && (
                <p className="text-xs text-muted-foreground mt-1">{t('apiKeys.noKeysHint')}</p>
              )}
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('apiKeys.keyPrefix')}</TableHead>
                  <TableHead>{t('apiKeys.surfaces')}</TableHead>
                  <TableHead>{t('apiKeys.expires')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('common.createdAt')}</TableHead>
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
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {(k.surfaces ?? []).map((s) => (
                          <Badge key={s} variant="outline" className="text-[10px]">
                            {t(`apiKeys.surfaceShort_${s}` as const)}
                          </Badge>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      <ExpiryCell apiKey={k} />
                    </TableCell>
                    <TableCell>
                      <StatusBadge apiKey={k} />
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(k.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <Button variant="ghost" size="icon-sm" onClick={() => openEditDialog(k)} title={t('common.edit')}>
                          <Pencil className="h-4 w-4" />
                        </Button>
                        {k.is_active && (
                          <>
                            <Button variant="ghost" size="icon-sm" onClick={() => openRotateDialog(k)} title={t('apiKeys.rotate')}>
                              <RotateCw className="h-4 w-4" />
                            </Button>
                            <Button variant="ghost" size="icon-sm" onClick={() => setRevokeTargetId(k.id)} title={t('common.revoke')}>
                              <Ban className="h-4 w-4" />
                            </Button>
                          </>
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
