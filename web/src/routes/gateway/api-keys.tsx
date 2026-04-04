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
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Plus, Copy, Check, Ban, RotateCw, Pencil, KeyRound, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ApiKey {
  id: string;
  name: string;
  key_prefix: string;
  team_name: string | null;
  user_id: string | null;
  team_id: string | null;
  allowed_models: string[] | null;
  rate_limit_rpm: number | null;
  rate_limit_tpm: number | null;
  expires_at: string | null;
  is_active: boolean;
  last_used_at: string | null;
  created_at: string;
  deleted_at: string | null;
  rotation_period_days: number | null;
  rotated_from_id: string | null;
  grace_period_ends_at: string | null;
  inactivity_timeout_days: number | null;
  disabled_reason: string | null;
  last_rotation_at: string | null;
}

interface PaginatedResponse<T> {
  data: T[];
  total: number;
  page: number;
  per_page: number;
}

interface CreateKeyResponse {
  id: string;
  api_key: string;
}

interface RotateKeyResponse {
  id: string;
  key: string;
  name: string;
  key_prefix: string;
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
  if (!apiKey.expires_at) return <span>{t('apiKeys.never')}</span>;

  const days = daysUntilExpiry(apiKey.expires_at);
  const dateStr = new Date(apiKey.expires_at).toLocaleDateString();

  if (days !== null && days < 0) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        <Badge variant="destructive" className="text-[10px] px-1 py-0">{t('apiKeys.expired')}</Badge>
      </span>
    );
  }

  if (days !== null && days < 1) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        <Badge variant="destructive" className="text-[10px] px-1 py-0">&lt;1d</Badge>
      </span>
    );
  }

  if (days !== null && days < 7) {
    return (
      <span className="flex items-center gap-1.5">
        {dateStr}
        <Badge className="bg-yellow-500/15 text-yellow-700 dark:text-yellow-400 border-yellow-500/30 text-[10px] px-1 py-0">
          {Math.ceil(days)}d
        </Badge>
      </span>
    );
  }

  return <span>{dateStr}</span>;
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

  // Create dialog
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [createdKey, setCreatedKey] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const [name, setName] = useState('');
  const [allowedModels, setAllowedModels] = useState('');
  const [rateLimitRpm, setRateLimitRpm] = useState('');
  const [expiresInDays, setExpiresInDays] = useState('');

  // Edit dialog
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editingKey, setEditingKey] = useState<ApiKey | null>(null);
  const [editAllowedModels, setEditAllowedModels] = useState('');
  const [editRateLimitRpm, setEditRateLimitRpm] = useState('');
  const [editExpiresInDays, setEditExpiresInDays] = useState('');
  const [editRotationPeriod, setEditRotationPeriod] = useState('');
  const [editInactivityTimeout, setEditInactivityTimeout] = useState('');
  const [editSubmitting, setEditSubmitting] = useState(false);
  const [editError, setEditError] = useState('');

  // Rotate dialog
  const [rotateDialogOpen, setRotateDialogOpen] = useState(false);
  const [rotatingKey, setRotatingKey] = useState<ApiKey | null>(null);
  const [rotatedNewKey, setRotatedNewKey] = useState<string | null>(null);
  const [rotateSubmitting, setRotateSubmitting] = useState(false);
  const [rotateError, setRotateError] = useState('');
  const [rotateCopied, setRotateCopied] = useState(false);
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
  // Create
  // ---------------------------------------------------------------------------

  const resetForm = () => {
    setName('');
    setAllowedModels('');
    setRateLimitRpm('');
    setExpiresInDays('');
    setFormError('');
    setCreatedKey(null);
    setCopied(false);
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      const models = allowedModels
        .split(',')
        .map((m) => m.trim())
        .filter(Boolean);
      const res = await apiPost<CreateKeyResponse>('/api/keys', {
        name,
        allowed_models: models.length > 0 ? models : undefined,
        rate_limit_rpm: rateLimitRpm ? parseInt(rateLimitRpm, 10) : undefined,
        expires_in_days: expiresInDays ? parseInt(expiresInDays, 10) : undefined,
      });
      setCreatedKey(res.api_key);
      await fetchKeys();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to create key');
    } finally {
      setSubmitting(false);
    }
  };

  const handleCopy = async () => {
    if (createdKey) {
      await navigator.clipboard.writeText(createdKey);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  };

  const handleDialogChange = (open: boolean) => {
    setDialogOpen(open);
    if (!open) resetForm();
  };

  // ---------------------------------------------------------------------------
  // Revoke
  // ---------------------------------------------------------------------------

  const handleRevoke = async (id: string) => {
    try {
      await apiDelete(`/api/keys/${id}`);
      setRevokeTargetId(null);
      toast.success(t('common.deleteSuccess'));
      await fetchKeys();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
    }
  };

  // ---------------------------------------------------------------------------
  // Edit
  // ---------------------------------------------------------------------------

  const openEditDialog = (k: ApiKey) => {
    setEditingKey(k);
    setEditAllowedModels(k.allowed_models?.join(', ') ?? '');
    setEditRateLimitRpm(k.rate_limit_rpm?.toString() ?? '');
    setEditExpiresInDays('');
    setEditRotationPeriod(k.rotation_period_days?.toString() ?? '');
    setEditInactivityTimeout(k.inactivity_timeout_days?.toString() ?? '');
    setEditError('');
    setEditDialogOpen(true);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!editingKey) return;
    setEditSubmitting(true);
    setEditError('');
    try {
      const models = editAllowedModels
        .split(',')
        .map((m) => m.trim())
        .filter(Boolean);
      await apiPatch(`/api/keys/${editingKey.id}`, {
        allowed_models: models.length > 0 ? models : null,
        rate_limit_rpm: editRateLimitRpm ? parseInt(editRateLimitRpm, 10) : null,
        expires_in_days: editExpiresInDays ? parseInt(editExpiresInDays, 10) : undefined,
        rotation_period_days: editRotationPeriod ? parseInt(editRotationPeriod, 10) : null,
        inactivity_timeout_days: editInactivityTimeout ? parseInt(editInactivityTimeout, 10) : null,
      });
      setEditDialogOpen(false);
      await fetchKeys();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed to update key');
    } finally {
      setEditSubmitting(false);
    }
  };

  // ---------------------------------------------------------------------------
  // Rotate
  // ---------------------------------------------------------------------------

  const openRotateDialog = (k: ApiKey) => {
    setRotatingKey(k);
    setRotatedNewKey(null);
    setRotateError('');
    setRotateCopied(false);
    setRotateDialogOpen(true);
  };

  const handleRotate = async () => {
    if (!rotatingKey) return;
    setRotateSubmitting(true);
    setRotateError('');
    try {
      const res = await apiPost<RotateKeyResponse>(`/api/keys/${rotatingKey.id}/rotate`, {});
      setRotatedNewKey(res.key);
      await fetchKeys();
    } catch (err) {
      setRotateError(err instanceof Error ? err.message : 'Failed to rotate key');
    } finally {
      setRotateSubmitting(false);
    }
  };

  const handleRotateCopy = async () => {
    if (rotatedNewKey) {
      await navigator.clipboard.writeText(rotatedNewKey);
      setRotateCopied(true);
      setTimeout(() => setRotateCopied(false), 2000);
    }
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
        <Dialog open={dialogOpen} onOpenChange={handleDialogChange}>
          <DialogTrigger asChild>
            <Button>
              <Plus className="h-4 w-4" />
              {t('apiKeys.createKey')}
            </Button>
          </DialogTrigger>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>{createdKey ? t('apiKeys.keyCreated') : t('apiKeys.createKey')}</DialogTitle>
              <DialogDescription>
                {createdKey ? t('apiKeys.keyCreatedHint') : t('apiKeys.dialogDescription')}
              </DialogDescription>
            </DialogHeader>
            {createdKey ? (
              <div className="space-y-4">
                <div className="rounded-md border bg-muted p-3">
                  <code className="text-sm break-all">{createdKey}</code>
                </div>
                <Button variant="outline" className="w-full" onClick={handleCopy}>
                  {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
                  {copied ? t('common.copied') : t('apiKeys.copyToClipboard')}
                </Button>
                <DialogFooter>
                  <Button onClick={() => handleDialogChange(false)}>{t('common.done')}</Button>
                </DialogFooter>
              </div>
            ) : (
              <form onSubmit={handleCreate} className="space-y-4">
                {formError && (
                  <Alert variant="destructive">
                    <AlertCircle className="h-4 w-4" />
                    <AlertDescription>{formError}</AlertDescription>
                  </Alert>
                )}
                <div className="space-y-2">
                  <Label htmlFor="key-name">{t('common.name')}</Label>
                  <Input id="key-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-service-key" required />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="key-models">{t('apiKeys.allowedModels')}</Label>
                  <Input id="key-models" value={allowedModels} onChange={(e) => setAllowedModels(e.target.value)} placeholder="gpt-4o, claude-sonnet-4 (comma-separated)" />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="key-rate">{t('apiKeys.rateLimitRpm')}</Label>
                  <Input id="key-rate" type="number" value={rateLimitRpm} onChange={(e) => setRateLimitRpm(e.target.value)} placeholder="60" />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="key-expires">{t('apiKeys.expiresInDays')}</Label>
                  <Input id="key-expires" type="number" value={expiresInDays} onChange={(e) => setExpiresInDays(e.target.value)} placeholder="90" />
                </div>
                <DialogFooter>
                  <Button type="submit" disabled={submitting}>
                    {submitting ? t('apiKeys.creating') : t('apiKeys.createKeyBtn')}
                  </Button>
                </DialogFooter>
              </form>
            )}
          </DialogContent>
        </Dialog>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {/* ---- Edit Dialog ---- */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('apiKeys.editKey')}</DialogTitle>
            <DialogDescription>{editingKey?.name ?? ''}</DialogDescription>
          </DialogHeader>
          <form onSubmit={handleEdit} className="space-y-4">
            {editError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{editError}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label>{t('apiKeys.allowedModels')}</Label>
              <Input value={editAllowedModels} onChange={(e) => setEditAllowedModels(e.target.value)} placeholder="gpt-4o, claude-sonnet-4" />
              <p className="text-xs text-muted-foreground">{t('apiKeys.allowedModelsHint')}</p>
            </div>
            <div className="space-y-2">
              <Label>{t('apiKeys.rateLimitRpm')}</Label>
              <Input type="number" value={editRateLimitRpm} onChange={(e) => setEditRateLimitRpm(e.target.value)} placeholder="60" />
            </div>
            <div className="space-y-2">
              <Label>{t('apiKeys.expiresIn')}</Label>
              <Input type="number" value={editExpiresInDays} onChange={(e) => setEditExpiresInDays(e.target.value)} placeholder="90" min={1} />
              <p className="text-xs text-muted-foreground">New expiry from now (days). Leave empty to keep current.</p>
            </div>
            <div className="space-y-2">
              <Label>{t('apiKeys.rotationPeriod')}</Label>
              <Input type="number" value={editRotationPeriod} onChange={(e) => setEditRotationPeriod(e.target.value)} placeholder="0" min={0} />
            </div>
            <div className="space-y-2">
              <Label>{t('apiKeys.inactivityTimeout')}</Label>
              <Input type="number" value={editInactivityTimeout} onChange={(e) => setEditInactivityTimeout(e.target.value)} placeholder="0" min={0} />
            </div>
            <DialogFooter>
              <Button type="submit" disabled={editSubmitting}>
                {editSubmitting ? t('common.loading') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* ---- Rotate Dialog ---- */}
      <Dialog open={rotateDialogOpen} onOpenChange={setRotateDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('apiKeys.rotate')}</DialogTitle>
            <DialogDescription>
              {rotatedNewKey ? t('apiKeys.rotateSuccess') : t('apiKeys.rotateConfirm')}
            </DialogDescription>
          </DialogHeader>
          {rotatedNewKey ? (
            <div className="space-y-4">
              <div className="rounded-md border bg-muted p-3">
                <code className="text-sm break-all">{rotatedNewKey}</code>
              </div>
              <Button variant="outline" className="w-full" onClick={handleRotateCopy}>
                {rotateCopied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
                {rotateCopied ? t('common.copied') : t('apiKeys.copyToClipboard')}
              </Button>
              <DialogFooter>
                <Button onClick={() => setRotateDialogOpen(false)}>{t('common.done')}</Button>
              </DialogFooter>
            </div>
          ) : (
            <div className="space-y-4">
              {rotateError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{rotateError}</AlertDescription>
                </Alert>
              )}
              <p className="text-sm text-muted-foreground">
                Key: <code className="rounded bg-muted px-1.5 py-0.5 text-xs">{rotatingKey?.key_prefix}</code> ({rotatingKey?.name})
              </p>
              <DialogFooter>
                <Button variant="outline" onClick={() => setRotateDialogOpen(false)}>
                  {t('common.cancel')}
                </Button>
                <Button onClick={handleRotate} disabled={rotateSubmitting}>
                  {rotateSubmitting ? t('common.loading') : t('apiKeys.rotate')}
                </Button>
              </DialogFooter>
            </div>
          )}
        </DialogContent>
      </Dialog>

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
                  <TableHead>{t('apiKeys.team')}</TableHead>
                  <TableHead>{t('apiKeys.rateLimit')}</TableHead>
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
                    <TableCell className="text-sm">{k.team_name ?? '—'}</TableCell>
                    <TableCell className="text-sm">{k.rate_limit_rpm ? `${k.rate_limit_rpm}/min` : '—'}</TableCell>
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

      <ConfirmDialog
        open={revokeTargetId !== null}
        onOpenChange={(open) => { if (!open) setRevokeTargetId(null); }}
        title={t('common.revoke')}
        description={t('apiKeys.revokeConfirm')}
        variant="destructive"
        confirmLabel={t('common.revoke')}
        onConfirm={() => { if (revokeTargetId) handleRevoke(revokeTargetId); }}
      />
    </div>
  );
}
