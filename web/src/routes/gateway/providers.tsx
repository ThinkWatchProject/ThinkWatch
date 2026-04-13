import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
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
import { Plus, Trash2, Pencil, X, Plug, AlertCircle, Zap, Loader2, CheckCircle2, XCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete, hasPermission } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { ScrollArea } from '@/components/ui/scroll-area';
import { toast } from 'sonner';

interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
  base_url: string;
  is_active: boolean;
  config_json?: { headers?: { key: string; value: string }[] };
  created_at: string;
}

const defaultHeadersForType = (type: string): [string, string][] => {
  switch (type) {
    case 'openai': return [['Authorization', 'Bearer ']];
    case 'anthropic': return [['x-api-key', ''], ['anthropic-version', '2023-06-01']];
    case 'google': return [['x-goog-api-key', '']];
    case 'azure_openai': return [['api-key', '']];
    case 'bedrock': return [['X-Aws-Access-Key-Id', ''], ['X-Aws-Secret-Access-Key', '']];
    default: return [];
  }
};

const providerTypeColors: Record<string, 'default' | 'secondary' | 'outline'> = {
  openai: 'default',
  anthropic: 'secondary',
  google: 'outline',
  azure_openai: 'default',
  bedrock: 'secondary',
  custom: 'outline',
};

export function ProvidersPage() {
  const { t } = useTranslation();
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const [name, setName] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [providerType, setProviderType] = useState('openai');
  const [baseUrl, setBaseUrl] = useState('');
  const [headers, setHeaders] = useState<[string, string][]>(defaultHeadersForType('openai'));

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editProvider, setEditProvider] = useState<Provider | null>(null);
  const [editDisplayName, setEditDisplayName] = useState('');
  const [editBaseUrl, setEditBaseUrl] = useState('');
  const [editHeaders, setEditHeaders] = useState<[string, string][]>([]);
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState('');
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  // Test connection state
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{
    success: boolean; message: string; latency_ms?: number;
    model_count?: number; models?: string[];
  } | null>(null);

  const handleTestConnection = async (
    type: string, url: string,
    hdrs: [string, string][],
  ) => {
    setTesting(true);
    setTestResult(null);
    try {
      const res = await apiPost<typeof testResult>(
        '/api/admin/providers/test',
        {
          provider_type: type,
          base_url: url,
          headers: hdrs.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
        },
      );
      setTestResult(res);
    } catch (err) {
      setTestResult({ success: false, message: err instanceof Error ? err.message : 'Connection failed' });
    } finally {
      setTesting(false);
    }
  };

  const fetchProviders = async () => {
    try {
      const data = await api<Provider[]>('/api/admin/providers');
      setProviders(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load providers');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchProviders(); }, []);

  const resetForm = () => {
    setName('');
    setDisplayName('');
    setProviderType('openai');
    setBaseUrl('');
    setHeaders(defaultHeadersForType('openai'));
    setFormError('');
    setTestResult(null);
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      await apiPost('/api/admin/providers', {
        name,
        display_name: displayName,
        provider_type: providerType,
        base_url: baseUrl,
        headers: headers.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
      });
      setDialogOpen(false);
      resetForm();
      await fetchProviders();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to create provider');
    } finally {
      setSubmitting(false);
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await apiDelete(`/api/admin/providers/${id}`);
      setDeleteTargetId(null);
      toast.success(t('common.deleteSuccess'));
      await fetchProviders();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
    }
  };

  const openEditDialog = (p: Provider) => {
    setEditProvider(p);
    setEditDisplayName(p.display_name);
    setEditBaseUrl(p.base_url);
    setEditError('');
    setTestResult(null);
    const existing = (p.config_json?.headers ?? []) as { key: string; value: string }[];
    setEditHeaders(existing.map(h => [h.key, h.value] as [string, string]));
    setEditDialogOpen(true);
  };

  const handleEdit = async () => {
    if (!editProvider) return;
    setEditError('');
    setEditSaving(true);
    try {
      await apiPatch(`/api/admin/providers/${editProvider.id}`, {
        display_name: editDisplayName,
        base_url: editBaseUrl,
        headers: editHeaders.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
      });
      setEditDialogOpen(false);
      setEditProvider(null);
      await fetchProviders();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed to update provider');
    } finally {
      setEditSaving(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('providers.title')}</h1>
          <p className="text-muted-foreground">{t('providers.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button disabled={!hasPermission('providers:create')}>
              <Plus className="h-4 w-4" />
              {t('providers.addProvider')}
            </Button>
          </DialogTrigger>
          <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
            <DialogHeader>
              <DialogTitle>{t('providers.addProvider')}</DialogTitle>
              <DialogDescription>{t('providers.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-4">
              {formError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{formError}</AlertDescription>
                </Alert>
              )}
              <div className="space-y-2">
                <Label htmlFor="prov-name">{t('common.name')}</Label>
                <Input id="prov-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-openai" required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-display">{t('providers.displayName')}</Label>
                <Input id="prov-display" value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="OpenAI Production" />
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-type">{t('providers.providerType')}</Label>
                <Select value={providerType} onValueChange={(v) => { setProviderType(v ?? 'openai'); setHeaders(defaultHeadersForType(v ?? 'openai')); }}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="openai">OpenAI</SelectItem>
                    <SelectItem value="anthropic">Anthropic</SelectItem>
                    <SelectItem value="google">Google Gemini</SelectItem>
                    <SelectItem value="azure_openai">Azure OpenAI</SelectItem>
                    <SelectItem value="bedrock">AWS Bedrock</SelectItem>
                    <SelectItem value="custom">Custom (OpenAI-compatible)</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-url">
                  {providerType === 'bedrock' ? t('providers.awsRegion') :
                   providerType === 'azure_openai' ? t('providers.azureEndpoint') :
                   t('providers.baseUrl')}
                </Label>
                <Input id="prov-url" value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={
                  providerType === 'azure_openai' ? 'https://your-resource.openai.azure.com' :
                  providerType === 'bedrock' ? 'us-east-1' :
                  providerType === 'anthropic' ? 'https://api.anthropic.com' :
                  providerType === 'google' ? 'https://generativelanguage.googleapis.com' :
                  'https://api.openai.com'
                } required />
              </div>
              <div className="space-y-2">
                <Label>{t('providers.headers')}</Label>
                <p className="text-xs text-muted-foreground">{t('providers.headersDesc')}</p>
                {headers.map(([k, v], i) => (
                  <div key={i} className="flex gap-2 items-center">
                    <Input className="flex-1" placeholder="Header-Name" value={k}
                      onChange={(e) => { const next = [...headers]; next[i] = [e.target.value, v]; setHeaders(next); }} />
                    <Input className="flex-1" type="text"
                      placeholder={t('mcpServers.headerValuePlaceholder')} value={v}
                      onChange={(e) => { const next = [...headers]; next[i] = [k, e.target.value]; setHeaders(next); }} />
                    <Button type="button" variant="ghost" size="icon-sm" onClick={() => setHeaders(headers.filter((_, j) => j !== i))}>
                      <X className="h-3 w-3" />
                    </Button>
                  </div>
                ))}
                <div className="flex flex-wrap gap-2">
                  <Button type="button" variant="outline" size="sm" onClick={() => setHeaders([...headers, ['', '']])}>
                    <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
                  </Button>
                  <Button type="button" variant="ghost" size="sm" className="text-xs text-muted-foreground" onClick={() => setHeaders([...headers, ['X-User-Id', '{{user_id}}']])}>
                    + {t('mcpServers.presetUserId')}
                  </Button>
                  <Button type="button" variant="ghost" size="sm" className="text-xs text-muted-foreground" onClick={() => setHeaders([...headers, ['X-User-Email', '{{user_email}}']])}>
                    + {t('mcpServers.presetUserEmail')}
                  </Button>
                </div>
              </div>
              {testResult && (
                <div className="space-y-2">
                  <Alert variant={testResult.success ? 'default' : 'destructive'}>
                    {testResult.success ? <CheckCircle2 className="h-4 w-4" /> : <XCircle className="h-4 w-4" />}
                    <AlertDescription>
                      {testResult.message}
                      {testResult.latency_ms != null && ` (${testResult.latency_ms}ms)`}
                    </AlertDescription>
                  </Alert>
                  {testResult.models && testResult.models.length > 0 && (
                    <ScrollArea className="h-32 rounded-md border p-2">
                      <ul className="space-y-0.5 text-xs font-mono">
                        {testResult.models.map((m) => <li key={m}>{m}</li>)}
                      </ul>
                    </ScrollArea>
                  )}
                </div>
              )}
              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  disabled={testing || !baseUrl}
                  onClick={() => handleTestConnection(providerType, baseUrl, headers)}
                >
                  {testing ? <Loader2 className="mr-1 h-4 w-4 animate-spin" /> : <Zap className="mr-1 h-4 w-4" />}
                  {testing ? t('providers.testing') : t('providers.testConnection')}
                </Button>
                <Button type="submit" disabled={submitting}>
                  {submitting ? t('providers.creating') : t('providers.createProvider')}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('providers.allProviders')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(3)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-5 w-16 rounded-full" />
                  <Skeleton className="h-4 w-48" />
                  <Skeleton className="h-5 w-14 rounded-full" />
                  <Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : providers.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <Plug className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('providers.noProviders')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('providers.noProvidersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('providers.type')}</TableHead>
                  <TableHead>{t('providers.baseUrl')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('providers.created')}</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {providers.map((p) => (
                  <TableRow key={p.id}>
                    <TableCell className="font-medium">{p.display_name || p.name}</TableCell>
                    <TableCell>
                      <Badge variant={providerTypeColors[p.provider_type] ?? 'outline'}>
                        {p.provider_type}
                      </Badge>
                    </TableCell>
                    <TableCell className="font-mono text-xs">{p.base_url}</TableCell>
                    <TableCell>
                      <Badge variant={p.is_active ? 'default' : 'destructive'}>
                        {p.is_active ? t('common.active') : t('common.inactive')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(p.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => openEditDialog(p)}
                          title={t('common.edit')}
                          disabled={!hasPermission('providers:update')}
                        >
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => setDeleteTargetId(p.id)}
                          title={t('common.delete')}
                          disabled={!hasPermission('providers:delete')}
                        >
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      {/* Edit Provider Dialog */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('providers.editProvider')}</DialogTitle>
            <DialogDescription>{t('providers.editDescription')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            {editError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{editError}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label>{t('providers.displayName')}</Label>
              <Input value={editDisplayName} onChange={(e) => setEditDisplayName(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.baseUrl')}</Label>
              <Input value={editBaseUrl} onChange={(e) => setEditBaseUrl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.headers')}</Label>
              <p className="text-xs text-muted-foreground">{t('providers.headersDesc')}</p>
              {editHeaders.map(([k, v], i) => (
                <div key={i} className="flex gap-2 items-center">
                  <Input className="flex-1" placeholder="Header-Name" value={k}
                    onChange={(e) => { const next = [...editHeaders]; next[i] = [e.target.value, v]; setEditHeaders(next); }} />
                  <Input className="flex-1" type="text"
                    placeholder={t('mcpServers.headerValuePlaceholder')} value={v}
                    onChange={(e) => { const next = [...editHeaders]; next[i] = [k, e.target.value]; setEditHeaders(next); }} />
                  <Button type="button" variant="ghost" size="icon-sm" onClick={() => setEditHeaders(editHeaders.filter((_, j) => j !== i))}>
                    <X className="h-3 w-3" />
                  </Button>
                </div>
              ))}
              <div className="flex flex-wrap gap-2">
                <Button type="button" variant="outline" size="sm" onClick={() => setEditHeaders([...editHeaders, ['', '']])}>
                  <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
                </Button>
                <Button type="button" variant="ghost" size="sm" className="text-xs text-muted-foreground" onClick={() => setEditHeaders([...editHeaders, ['X-User-Id', '{{user_id}}']])}>
                  + {t('mcpServers.presetUserId')}
                </Button>
                <Button type="button" variant="ghost" size="sm" className="text-xs text-muted-foreground" onClick={() => setEditHeaders([...editHeaders, ['X-User-Email', '{{user_email}}']])}>
                  + {t('mcpServers.presetUserEmail')}
                </Button>
              </div>
            </div>
          </div>
          {testResult && (
            <div className="space-y-2">
              <Alert variant={testResult.success ? 'default' : 'destructive'}>
                {testResult.success ? <CheckCircle2 className="h-4 w-4" /> : <XCircle className="h-4 w-4" />}
                <AlertDescription>
                  {testResult.message}
                  {testResult.latency_ms != null && ` (${testResult.latency_ms}ms)`}
                </AlertDescription>
              </Alert>
              {testResult.models && testResult.models.length > 0 && (
                <ScrollArea className="h-32 rounded-md border p-2">
                  <ul className="space-y-0.5 text-xs font-mono">
                    {testResult.models.map((m) => <li key={m}>{m}</li>)}
                  </ul>
                </ScrollArea>
              )}
            </div>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setEditDialogOpen(false)}>{t('common.cancel')}</Button>
            <Button
              type="button"
              variant="outline"
              disabled={testing || !editBaseUrl}
              onClick={() => handleTestConnection(editProvider!.provider_type, editBaseUrl, editHeaders)}
            >
              {testing ? <Loader2 className="mr-1 h-4 w-4 animate-spin" /> : <Zap className="mr-1 h-4 w-4" />}
              {testing ? t('providers.testing') : t('providers.testConnection')}
            </Button>
            <Button onClick={handleEdit} disabled={editSaving}>
              {editSaving ? t('common.loading') : t('common.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteTargetId !== null}
        onOpenChange={(open) => { if (!open) setDeleteTargetId(null); }}
        title={t('common.delete')}
        description={t('providers.deleteConfirm')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={() => { if (deleteTargetId) handleDelete(deleteTargetId); }}
      />
    </div>
  );
}
