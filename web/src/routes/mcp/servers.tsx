import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { StatusIndicator } from '@/components/ui/status-indicator';
import { TransportBadge } from '@/components/ui/transport-badge';
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
import { Plus, Trash2, Pencil, Server, AlertCircle, Zap, Loader2, CheckCircle2, XCircle, RefreshCw } from 'lucide-react';
import { HeaderEditor } from '@/components/header-editor';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { slugifyPrefix, resolveCollision, sanitizePrefixInput } from '@/lib/prefix-utils';
import { ScrollArea } from '@/components/ui/scroll-area';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';

interface McpServer {
  id: string;
  name: string;
  namespace_prefix: string;
  description: string | null;
  endpoint_url: string;
  transport_type: string;
  auth_type: string | null;
  status: string;
  last_health_check: string | null;
  tools_count: number;
  call_count: number;
  config_json?: { custom_headers?: Record<string, string>; cache_ttl_secs?: number };
  created_at: string;
}

export function McpServersPage() {
  const { t } = useTranslation();
  const [servers, setServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);
  const pager = useClientPagination(servers, 20);
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const [name, setName] = useState('');
  const [namespacePrefix, setNamespacePrefix] = useState('');
  // When true, stop auto-deriving `namespacePrefix` from `name` — the user
  // has taken manual control of the prefix field.
  const [prefixManuallyEdited, setPrefixManuallyEdited] = useState(false);
  const [description, setDescription] = useState('');
  const [endpointUrl, setEndpointUrl] = useState('');
  const [authType, setAuthType] = useState('none');
  const [authSecret, setAuthSecret] = useState('');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>([]);
  const [cacheTtl, setCacheTtl] = useState('');

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editServer, setEditServer] = useState<McpServer | null>(null);
  const [editName, setEditName] = useState('');
  const [editDescription, setEditDescription] = useState('');
  const [editEndpointUrl, setEditEndpointUrl] = useState('');
  const [editNamespacePrefix, setEditNamespacePrefix] = useState('');
  const [editAuthType, setEditAuthType] = useState('none');
  const [editAuthSecret, setEditAuthSecret] = useState('');
  const [editCustomHeaders, setEditCustomHeaders] = useState<[string, string][]>([]);
  const [editCacheTtl, setEditCacheTtl] = useState('');
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState('');
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  // Test connection state
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{
    success: boolean; message: string; latency_ms?: number;
    tools_count?: number; tools?: { name: string; description?: string }[];
  } | null>(null);

  const handleTestConnection = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      const res = await apiPost<typeof testResult>('/api/mcp/servers/test', {
        endpoint_url: endpointUrl,
        auth_type: authType,
        auth_secret: authSecret || undefined,
        custom_headers: customHeaders.length > 0
          ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
          : null,
      });
      setTestResult(res);
    } catch (err) {
      setTestResult({ success: false, message: err instanceof Error ? err.message : 'Connection failed' });
    } finally {
      setTesting(false);
    }
  };

  const fetchServers = async (signal?: AbortSignal) => {
    try {
      const data = await api<McpServer[]>('/api/mcp/servers', { signal });
      setServers(data);
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : 'Failed to load MCP servers');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    const controller = new AbortController();
    fetchServers(controller.signal);
    return () => controller.abort();
  }, []);

  // Live preview of the name/prefix that will actually be written to the DB.
  // Resolves collisions against already-registered servers by appending
  // `#2`, `_2`, etc. — mirrors the backend's `resolve_server_collisions`.
  const taken = useMemo(() => ({
    names: new Set(servers.map((s) => s.name)),
    prefixes: new Set(servers.map((s) => s.namespace_prefix)),
  }), [servers]);

  const resolved = useMemo(() => {
    if (!name.trim()) return null;
    const basePrefix = prefixManuallyEdited && namespacePrefix
      ? namespacePrefix
      : slugifyPrefix(name);
    if (!basePrefix) return null;
    return resolveCollision(name.trim(), basePrefix, taken.names, taken.prefixes);
  }, [name, namespacePrefix, prefixManuallyEdited, taken]);

  const resetForm = () => {
    setName('');
    setNamespacePrefix('');
    setPrefixManuallyEdited(false);
    setDescription('');
    setEndpointUrl('');
    setAuthType('none');
    setAuthSecret('');
    setCustomHeaders([]);
    setCacheTtl('');
    setFormError('');
    setTestResult(null);
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      // Test connection first — refuse to save if we can't list tools
      const headers = customHeaders.length > 0
        ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
        : null;
      const test = await apiPost<{ success: boolean; message: string; tools_count?: number }>(
        '/api/mcp/servers/test',
        {
          endpoint_url: endpointUrl,
          auth_type: authType,
          auth_secret: authSecret || undefined,
          custom_headers: headers,
        },
      );
      setTestResult(test);
      if (!test.success) {
        setFormError(t('mcpServers.testFailedBlocking', { msg: test.message }));
        return;
      }

      await apiPost('/api/mcp/servers', {
        name: resolved?.name ?? name,
        namespace_prefix: resolved?.prefix ?? (namespacePrefix || undefined),
        description,
        endpoint_url: endpointUrl,
        auth_type: authType,
        auth_secret: authSecret || undefined,
        custom_headers: headers,
        cache_ttl_secs: cacheTtl ? Number(cacheTtl) : undefined,
      });
      setDialogOpen(false);
      resetForm();
      await fetchServers();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to register server');
    } finally {
      setSubmitting(false);
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await apiDelete(`/api/mcp/servers/${id}`);
      setDeleteTargetId(null);
      toast.success(t('common.deleteSuccess'));
      await fetchServers();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
    }
  };

  const [discoveringId, setDiscoveringId] = useState<string | null>(null);

  const handleDiscover = async (id: string) => {
    setDiscoveringId(id);
    try {
      const res = await apiPost<{ tools_discovered: number }>(`/api/mcp/servers/${id}/discover`, {});
      toast.success(t('mcpServers.discoverTools') + `: ${res.tools_discovered} tools`);
      await fetchServers();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to discover tools');
    } finally {
      setDiscoveringId(null);
    }
  };

  const openEditDialog = (s: McpServer) => {
    setEditServer(s);
    setEditName(s.name);
    setEditDescription(s.description ?? '');
    setEditEndpointUrl(s.endpoint_url);
    setEditNamespacePrefix(s.namespace_prefix ?? '');
    setEditAuthType(s.auth_type ?? 'none');
    setEditAuthSecret('');
    setEditError('');
    const existing = s.config_json?.custom_headers ?? {};
    setEditCustomHeaders(Object.entries(existing));
    setEditCacheTtl(s.config_json?.cache_ttl_secs != null ? String(s.config_json.cache_ttl_secs) : '');
    setEditDialogOpen(true);
  };

  const handleEdit = async () => {
    if (!editServer) return;
    setEditError('');
    setEditSaving(true);
    try {
      const headers = editCustomHeaders.length > 0
        ? Object.fromEntries(editCustomHeaders.filter(([k]) => k.trim()))
        : {};

      // Test connection first — refuse to save if we can't list tools
      const test = await apiPost<{ success: boolean; message: string }>(
        '/api/mcp/servers/test',
        {
          endpoint_url: editEndpointUrl,
          auth_type: editAuthType,
          // If user didn't re-enter the secret, the backend will keep the
          // existing one during save. But test needs a value to actually
          // probe auth, so only pass when provided.
          auth_secret: editAuthSecret || undefined,
          custom_headers: headers,
          // Fall back to stored credentials when the secret field is empty
          server_id: editServer.id,
        },
      );
      if (!test.success) {
        setEditError(t('mcpServers.testFailedBlocking', { msg: test.message }));
        return;
      }

      await apiPatch(`/api/mcp/servers/${editServer.id}`, {
        name: editName,
        namespace_prefix: editNamespacePrefix || undefined,
        description: editDescription,
        endpoint_url: editEndpointUrl,
        auth_type: editAuthType,
        auth_secret: editAuthSecret || undefined,
        custom_headers: headers,
        cache_ttl_secs: editCacheTtl ? Number(editCacheTtl) : undefined,
      });
      setEditDialogOpen(false);
      setEditServer(null);
      await fetchServers();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed to update server');
    } finally {
      setEditSaving(false);
    }
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('mcpServers.title')}</h1>
          <p className="text-muted-foreground">{t('mcpServers.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button>
              <Plus className="h-4 w-4" />
              {t('mcpServers.registerServer')}
            </Button>
          </DialogTrigger>
          <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
            <DialogHeader>
              <DialogTitle>{t('mcpServers.dialogTitle')}</DialogTitle>
              <DialogDescription>{t('mcpServers.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-4">
              {formError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{formError}</AlertDescription>
                </Alert>
              )}
              <div className="space-y-2">
                <Label htmlFor="mcp-name">{t('common.name')}</Label>
                <Input id="mcp-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-mcp-server" required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-prefix">{t('mcpServers.namespacePrefix')}</Label>
                <Input
                  id="mcp-prefix"
                  value={prefixManuallyEdited ? namespacePrefix : (resolved?.prefix ?? slugifyPrefix(name))}
                  onChange={(e) => {
                    setPrefixManuallyEdited(true);
                    setNamespacePrefix(sanitizePrefixInput(e.target.value));
                  }}
                  placeholder={t('mcpServers.namespacePrefixPlaceholder')}
                  pattern="[a-z0-9_]{1,32}"
                  maxLength={32}
                />
                {resolved && (
                  <p className="text-xs text-muted-foreground">
                    {t('mcpServers.willBeStoredAs')}{' '}
                    <code className="rounded bg-muted px-1 font-mono">{resolved.name}</code>
                    {' / '}
                    <code className="rounded bg-muted px-1 font-mono">{resolved.prefix}</code>
                  </p>
                )}
                <p className="text-xs text-muted-foreground">{t('mcpServers.namespacePrefixHint')}</p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-desc">{t('common.description')}</Label>
                <Input id="mcp-desc" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Code analysis tools" />
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-url">{t('mcpServers.endpointUrl')}</Label>
                <Input id="mcp-url" value={endpointUrl} onChange={(e) => setEndpointUrl(e.target.value)} placeholder="http://localhost:8081/mcp" required />
              </div>
              <div className="space-y-2">
                <Label>{t('mcpServers.authType')}</Label>
                <Select value={authType} onValueChange={(v) => { if (v) setAuthType(v); }}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="none">None</SelectItem>
                    <SelectItem value="bearer">Bearer Token</SelectItem>
                    <SelectItem value="api_key">API Key</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              {authType !== 'none' && (
                <div className="space-y-2">
                  <Label htmlFor="mcp-secret">{t('mcpServers.authSecret')}</Label>
                  <Input id="mcp-secret" type="password" value={authSecret} onChange={(e) => setAuthSecret(e.target.value)} placeholder="Secret or token" required />
                </div>
              )}
              <div className="space-y-2">
                <Label>{t('mcpServers.cacheTtlLabel')}</Label>
                <p className="text-xs text-muted-foreground">{t('mcpServers.cacheTtlHint')}</p>
                <Input type="number" min={0} step={60} placeholder={t('mcpServers.cacheTtlPlaceholder')} value={cacheTtl} onChange={(e) => setCacheTtl(e.target.value)} />
              </div>
              <div className="space-y-2">
                <Label>{t('providers.customHeaders')}</Label>
                <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
                <HeaderEditor
                  headers={customHeaders}
                  onChange={setCustomHeaders}
                  keyPlaceholder="X-Custom-Header"
                  presets={[
                    { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                    { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
                  ]}
                />
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
                  {testResult.tools && testResult.tools.length > 0 && (
                    <ScrollArea className="h-32 rounded-md border p-2">
                      <ul className="space-y-1 text-xs">
                        {testResult.tools.map((tool) => (
                          <li key={tool.name} className="flex items-baseline gap-2">
                            <code className="font-medium">{tool.name}</code>
                            {tool.description && <span className="text-muted-foreground truncate">{tool.description}</span>}
                          </li>
                        ))}
                      </ul>
                    </ScrollArea>
                  )}
                </div>
              )}
              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  disabled={testing || !endpointUrl}
                  onClick={handleTestConnection}
                >
                  {testing ? <Loader2 className="mr-1 h-4 w-4 animate-spin" /> : <Zap className="mr-1 h-4 w-4" />}
                  {testing ? t('providers.testing') : t('providers.testConnection')}
                </Button>
                {/* Submit is gated on a successful test-connection so the
                    admin always sees the tool list before a server is
                    persisted — avoids the "registered then discovered
                    it's broken" footgun we hit in the earlier session. */}
                <Button
                  type="submit"
                  disabled={submitting || !testResult?.success}
                  title={!testResult?.success ? t('mcpServers.mustTestFirst') : undefined}
                >
                  {submitting ? t('mcpServers.registering') : t('mcpServers.registerServer')}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-3 p-4">
              {[...Array(3)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-4 w-48" />
                  <Skeleton className="h-5 w-20 rounded-full" />
                  <Skeleton className="h-5 w-16 rounded-full" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-8" />
                </div>
              ))}
            </div>
          ) : servers.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center text-center">
              <Server className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('mcpServers.noServers')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('mcpServers.noServersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('mcpServers.endpointUrl')}</TableHead>
                  <TableHead>{t('mcpServers.transport')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('mcpServers.lastHealthCheck')}</TableHead>
                  <TableHead>{t('mcpServers.toolsCount')}</TableHead>
                  <TableHead>{t('mcpServers.callsCount')}</TableHead>
                  <TableHead className="w-20" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {pager.paginated.map((s) => (
                  <TableRow key={s.id}>
                    <TableCell className="font-medium">{s.name}</TableCell>
                    <TableCell className="font-mono text-xs">{s.endpoint_url}</TableCell>
                    <TableCell>
                      <TransportBadge transport={s.transport_type} />
                    </TableCell>
                    <TableCell>
                      <StatusIndicator
                        status={s.status === 'connected' ? 'healthy' : s.status === 'disconnected' ? 'down' : 'unknown'}
                        label={t(`common.${s.status === 'connected' ? 'healthy' : s.status === 'disconnected' ? 'down' : 'unknown'}`, s.status)}
                        showLabel
                        pulse
                      />
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {s.last_health_check ? new Date(s.last_health_check).toLocaleString() : '—'}
                    </TableCell>
                    <TableCell className="text-sm">{s.tools_count}</TableCell>
                    <TableCell className="font-mono text-xs tabular-nums text-muted-foreground">
                      {(s.call_count ?? 0).toLocaleString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button variant="ghost" size="icon-sm" onClick={() => openEditDialog(s)} title={t('common.edit')}>
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button variant="ghost" size="icon-sm" onClick={() => handleDiscover(s.id)} disabled={discoveringId === s.id} title={t('mcpServers.discoverTools')}>
                          {discoveringId === s.id ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
                        </Button>
                        <Button variant="ghost" size="icon-sm" onClick={() => setDeleteTargetId(s.id)} title={t('common.delete')}>
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
        <div data-slot="card-footer" className="border-t">
          <DataTablePagination
            total={pager.total}
            page={pager.page}
            pageSize={pager.pageSize}
            onPageChange={pager.setPage}
            onPageSizeChange={pager.setPageSize}
          />
        </div>
      </Card>

      {/* Edit MCP Server Dialog */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('mcpServers.editServer')}</DialogTitle>
            <DialogDescription>{t('mcpServers.editDescription')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            {editError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{editError}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label htmlFor="edit-mcp-name">{t('common.name')}</Label>
              <Input id="edit-mcp-name" value={editName} onChange={(e) => setEditName(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-mcp-prefix">{t('mcpServers.namespacePrefix')}</Label>
              <Input
                id="edit-mcp-prefix"
                value={editNamespacePrefix}
                onChange={(e) => setEditNamespacePrefix(e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, '_'))}
                pattern="[a-z0-9_]{1,32}"
                maxLength={32}
              />
              <p className="text-xs text-muted-foreground">{t('mcpServers.namespacePrefixHint')}</p>
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-mcp-desc">{t('common.description')}</Label>
              <Input id="edit-mcp-desc" value={editDescription} onChange={(e) => setEditDescription(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-mcp-url">{t('mcpServers.endpointUrl')}</Label>
              <Input id="edit-mcp-url" value={editEndpointUrl} onChange={(e) => setEditEndpointUrl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('mcpServers.authType')}</Label>
              <Select value={editAuthType} onValueChange={(v) => { if (v) setEditAuthType(v); }}>
                <SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="none">None</SelectItem>
                  <SelectItem value="bearer">Bearer Token</SelectItem>
                  <SelectItem value="api_key">API Key</SelectItem>
                </SelectContent>
              </Select>
            </div>
            {editAuthType !== 'none' && (
              <div className="space-y-2">
                <Label>{t('mcpServers.authSecret')}</Label>
                <Input type="password" value={editAuthSecret} onChange={(e) => setEditAuthSecret(e.target.value)} placeholder="Leave empty to keep current" />
              </div>
            )}
            <div className="space-y-2">
              <Label>{t('mcpServers.cacheTtlLabel')}</Label>
              <p className="text-xs text-muted-foreground">{t('mcpServers.cacheTtlHint')}</p>
              <Input type="number" min={0} step={60} placeholder={t('mcpServers.cacheTtlPlaceholder')} value={editCacheTtl} onChange={(e) => setEditCacheTtl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.customHeaders')}</Label>
              <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
              <HeaderEditor
                headers={editCustomHeaders}
                onChange={setEditCustomHeaders}
                keyPlaceholder="X-Custom-Header"
                presets={[
                  { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                  { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
                ]}
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setEditDialogOpen(false)}>{t('common.cancel')}</Button>
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
        description={t('mcpServers.deleteConfirm')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={() => { if (deleteTargetId) handleDelete(deleteTargetId); }}
      />
    </div>
  );
}
