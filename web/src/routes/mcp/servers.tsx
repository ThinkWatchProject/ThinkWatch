import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { StatusIndicator } from '@/components/ui/status-indicator';
import { TransportBadge } from '@/components/ui/transport-badge';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
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
import { Plus, Trash2, Pencil, Server, AlertCircle, Loader2, RefreshCw } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiPost, apiDelete, hasPermission } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';
import { ServerWizard } from '@/components/mcp/server-wizard';
import { ServerEditForm } from '@/components/mcp/server-edit-form';
import { AuthModeBadge } from '@/components/mcp/auth-mode-badge';
import { deriveAuthMode } from '@/components/mcp/auth-mode-utils';

interface McpServer {
  id: string;
  name: string;
  namespace_prefix: string;
  description: string | null;
  endpoint_url: string;
  transport_type: string;
  oauth_issuer: string | null;
  oauth_authorization_endpoint: string | null;
  oauth_token_endpoint: string | null;
  oauth_revocation_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  oauth_client_id: string | null;
  oauth_scopes: string[];
  allow_static_token: boolean;
  static_token_help_url: string | null;
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

  const [editServer, setEditServer] = useState<McpServer | null>(null);
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);
  const [discoveringId, setDiscoveringId] = useState<string | null>(null);

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

  const taken = useMemo(() => ({
    names: new Set(servers.map((s) => s.name)),
    prefixes: new Set(servers.map((s) => s.namespace_prefix)),
  }), [servers]);

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

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('mcpServers.title')}</h1>
          <p className="text-muted-foreground">{t('mcpServers.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button disabled={!hasPermission('mcp_servers:create')}>
              <Plus className="h-4 w-4" />
              {t('mcpServers.registerServer')}
            </Button>
          </DialogTrigger>
          <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
            <DialogHeader>
              <DialogTitle>{t('mcpServers.dialogTitle')}</DialogTitle>
              <DialogDescription>{t('mcpServers.dialogDescription')}</DialogDescription>
            </DialogHeader>
            {dialogOpen && (
              <ServerWizard
                taken={taken}
                onCancel={() => setDialogOpen(false)}
                onSuccess={() => {
                  setDialogOpen(false);
                  void fetchServers();
                }}
              />
            )}
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
                  <TableHead className="w-12">{t('mcpServers.authMode')}</TableHead>
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
                      <AuthModeBadge mode={deriveAuthMode(s)} compact />
                    </TableCell>
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
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => setEditServer(s)}
                          title={t('common.edit')}
                          disabled={!hasPermission('mcp_servers:update')}
                        >
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => handleDiscover(s.id)}
                          disabled={
                            discoveringId === s.id || !hasPermission('mcp_servers:update')
                          }
                          title={t('mcpServers.discoverTools')}
                        >
                          {discoveringId === s.id ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => setDeleteTargetId(s.id)}
                          title={t('common.delete')}
                          disabled={!hasPermission('mcp_servers:delete')}
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

      <Dialog open={editServer !== null} onOpenChange={(open) => { if (!open) setEditServer(null); }}>
        <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{t('mcpServers.editServer')}</DialogTitle>
            <DialogDescription>{t('mcpServers.editDescription')}</DialogDescription>
          </DialogHeader>
          {editServer && (
            <ServerEditForm
              server={editServer}
              onCancel={() => setEditServer(null)}
              onSaved={() => {
                setEditServer(null);
                void fetchServers();
              }}
            />
          )}
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
