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
import { Plus, Trash2, Search, Pencil, X, Server, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';

interface McpServer {
  id: string;
  name: string;
  description: string;
  endpoint_url: string;
  transport_type: string;
  status: string;
  last_health_check: string | null;
  tools_count: number;
  config_json?: { custom_headers?: Record<string, string> };
  created_at: string;
}

const statusVariants: Record<string, 'default' | 'secondary' | 'destructive' | 'outline'> = {
  connected: 'default',
  disconnected: 'destructive',
  pending: 'outline',
};

export function McpServersPage() {
  const { t } = useTranslation();
  const [servers, setServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [endpointUrl, setEndpointUrl] = useState('');
  const [transportType, setTransportType] = useState('streamable_http');
  const [authType, setAuthType] = useState('none');
  const [authSecret, setAuthSecret] = useState('');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>([]);

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editServer, setEditServer] = useState<McpServer | null>(null);
  const [editName, setEditName] = useState('');
  const [editDescription, setEditDescription] = useState('');
  const [editEndpointUrl, setEditEndpointUrl] = useState('');
  const [editCustomHeaders, setEditCustomHeaders] = useState<[string, string][]>([]);
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState('');
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  const fetchServers = async () => {
    try {
      const data = await api<McpServer[]>('/api/mcp/servers');
      setServers(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load MCP servers');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchServers(); }, []);

  const resetForm = () => {
    setName('');
    setDescription('');
    setEndpointUrl('');
    setTransportType('streamable_http');
    setAuthType('none');
    setAuthSecret('');
    setCustomHeaders([]);
    setFormError('');
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      await apiPost('/api/mcp/servers', {
        name,
        description,
        endpoint_url: endpointUrl,
        transport_type: transportType,
        auth_type: authType,
        auth_secret: authSecret || undefined,
        custom_headers: customHeaders.length > 0
          ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
          : null,
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

  const handleDiscover = async (id: string) => {
    try {
      await apiPost(`/api/mcp/servers/${id}/discover`, {});
      await fetchServers();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to discover tools');
    }
  };

  const openEditDialog = (s: McpServer) => {
    setEditServer(s);
    setEditName(s.name);
    setEditDescription(s.description);
    setEditEndpointUrl(s.endpoint_url);
    setEditError('');
    const existing = s.config_json?.custom_headers ?? {};
    setEditCustomHeaders(Object.entries(existing));
    setEditDialogOpen(true);
  };

  const handleEdit = async () => {
    if (!editServer) return;
    setEditError('');
    setEditSaving(true);
    try {
      await apiPatch(`/api/mcp/servers/${editServer.id}`, {
        name: editName,
        description: editDescription,
        endpoint_url: editEndpointUrl,
        custom_headers: editCustomHeaders.length > 0
          ? Object.fromEntries(editCustomHeaders.filter(([k]) => k.trim()))
          : {},
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
    <div className="space-y-6">
      <div className="flex items-center justify-between">
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
                <Label htmlFor="mcp-desc">{t('common.description')}</Label>
                <Input id="mcp-desc" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Code analysis tools" />
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-url">{t('mcpServers.endpointUrl')}</Label>
                <Input id="mcp-url" value={endpointUrl} onChange={(e) => setEndpointUrl(e.target.value)} placeholder="http://localhost:8081/mcp" required />
              </div>
              <div className="space-y-2">
                <Label>{t('mcpServers.transportType')}</Label>
                <Select value={transportType} onValueChange={(v) => { if (v) setTransportType(v); }}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="streamable_http">Streamable HTTP</SelectItem>
                    <SelectItem value="sse">SSE</SelectItem>
                    <SelectItem value="stdio">Stdio</SelectItem>
                  </SelectContent>
                </Select>
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
                <Label>{t('providers.customHeaders')}</Label>
                <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
                {customHeaders.map(([k, v], i) => (
                  <div key={i} className="flex gap-2 items-center">
                    <Input className="flex-1" placeholder="Header-Name" value={k}
                      onChange={(e) => { const next = [...customHeaders]; next[i] = [e.target.value, v]; setCustomHeaders(next); }} />
                    <Input className="flex-1" placeholder="value" value={v}
                      onChange={(e) => { const next = [...customHeaders]; next[i] = [k, e.target.value]; setCustomHeaders(next); }} />
                    <Button type="button" variant="ghost" size="icon-sm" onClick={() => setCustomHeaders(customHeaders.filter((_, j) => j !== i))}>
                      <X className="h-3 w-3" />
                    </Button>
                  </div>
                ))}
                <Button type="button" variant="outline" size="sm" onClick={() => setCustomHeaders([...customHeaders, ['', '']])}>
                  <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
                </Button>
              </div>
              <DialogFooter>
                <Button type="submit" disabled={submitting}>
                  {submitting ? t('mcpServers.registering') : t('mcpServers.registerServer')}
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
          <CardTitle className="text-base">{t('mcpServers.allServers')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
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
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <Server className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('mcpServers.noServers')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('mcpServers.noServersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('mcpServers.endpointUrl')}</TableHead>
                  <TableHead>{t('mcpServers.transport')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('mcpServers.lastHealthCheck')}</TableHead>
                  <TableHead>{t('mcpServers.toolsCount')}</TableHead>
                  <TableHead className="w-20" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {servers.map((s) => (
                  <TableRow key={s.id}>
                    <TableCell className="font-medium">{s.name}</TableCell>
                    <TableCell className="font-mono text-xs">{s.endpoint_url}</TableCell>
                    <TableCell>
                      <Badge variant="outline">{s.transport_type}</Badge>
                    </TableCell>
                    <TableCell>
                      <Badge variant={statusVariants[s.status] ?? 'outline'}>
                        {s.status}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {s.last_health_check ? new Date(s.last_health_check).toLocaleString() : '—'}
                    </TableCell>
                    <TableCell className="text-sm">{s.tools_count}</TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button variant="ghost" size="icon-sm" onClick={() => openEditDialog(s)} title={t('common.edit')}>
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button variant="ghost" size="icon-sm" onClick={() => handleDiscover(s.id)} title={t('mcpServers.discoverTools')}>
                          <Search className="h-4 w-4" />
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
      </Card>

      {/* Edit MCP Server Dialog */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
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
              <Label htmlFor="edit-mcp-desc">{t('common.description')}</Label>
              <Input id="edit-mcp-desc" value={editDescription} onChange={(e) => setEditDescription(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-mcp-url">{t('mcpServers.endpointUrl')}</Label>
              <Input id="edit-mcp-url" value={editEndpointUrl} onChange={(e) => setEditEndpointUrl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.customHeaders')}</Label>
              <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
              {editCustomHeaders.map(([k, v], i) => (
                <div key={i} className="flex gap-2 items-center">
                  <Input className="flex-1" placeholder="Header-Name" value={k}
                    onChange={(e) => { const next = [...editCustomHeaders]; next[i] = [e.target.value, v]; setEditCustomHeaders(next); }} />
                  <Input className="flex-1" placeholder="value" value={v}
                    onChange={(e) => { const next = [...editCustomHeaders]; next[i] = [k, e.target.value]; setEditCustomHeaders(next); }} />
                  <Button type="button" variant="ghost" size="icon-sm" onClick={() => setEditCustomHeaders(editCustomHeaders.filter((_, j) => j !== i))}>
                    <X className="h-3 w-3" />
                  </Button>
                </div>
              ))}
              <Button type="button" variant="outline" size="sm" onClick={() => setEditCustomHeaders([...editCustomHeaders, ['', '']])}>
                <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
              </Button>
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
