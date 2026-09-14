import { useEffect, useState } from 'react';
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
import { Checkbox } from '@/components/ui/checkbox';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';
import { Link } from '@tanstack/react-router';
import { ServerEditForm } from '@/components/mcp/server-edit-form';
import { AuthModeBadge } from '@/components/mcp/auth-mode-badge';
import { deriveAuthMode } from '@/components/mcp/auth-mode-utils';
import { CredentialOwnerBadge } from '@/components/mcp/credential-owner-badge';

interface McpServer {
  id: string;
  name: string;
  namespace_prefix: string;
  display_label: string | null;
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
  auth_shape: 'anonymous' | 'oauth' | 'static';
  static_token_help_url: string | null;
  auth_header_name: string;
  auth_value_template: string;
  credential_owner: 'per_user' | 'admin_shared';
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

  const [editServer, setEditServer] = useState<McpServer | null>(null);
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);
  const [discoveringId, setDiscoveringId] = useState<string | null>(null);

  // Multi-select for bulk delete. Stores server `id` (UUID), matches
  // the `POST /api/mcp/servers/bulk-delete` payload contract.
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false);
  const [bulkDeleting, setBulkDeleting] = useState(false);

  const fetchServers = async (signal?: AbortSignal) => {
    try {
      const data = await api<McpServer[]>('/api/mcp/servers', { signal });
      setServers(data);
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    const controller = new AbortController();
    // Async loader: its first statement is the `await`, so every setState
    // inside runs in the continuation — never synchronously with this
    // effect, and never as a cascading render. The rule's cross-function
    // analysis does not model `await`.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    fetchServers(controller.signal);
    return () => controller.abort();
  }, []);

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

  // Selection helpers. Header checkbox is tri-state: every row of the
  // CURRENT page picked = checked, some picked = indeterminate, none
  // picked = unchecked. We scope the "select all" to the current page
  // (`pager.paginated`) rather than the full server list — admins
  // browsing page 3 don't expect ticking the header to silently grab
  // pages 1-2 as well.
  const visibleServers = pager.paginated;
  const allVisibleSelected =
    visibleServers.length > 0 && visibleServers.every((s) => selectedIds.has(s.id));
  const someVisibleSelected =
    !allVisibleSelected && visibleServers.some((s) => selectedIds.has(s.id));
  const headerCheckState: boolean | 'indeterminate' = allVisibleSelected
    ? true
    : someVisibleSelected
      ? 'indeterminate'
      : false;

  const toggleSelectAll = () => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (allVisibleSelected) for (const s of visibleServers) next.delete(s.id);
      else for (const s of visibleServers) next.add(s.id);
      return next;
    });
  };

  const toggleSelect = (id: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const confirmBulkDelete = async () => {
    if (selectedIds.size === 0) return;
    setBulkDeleting(true);
    try {
      const res = await apiPost<{
        deleted: string[];
        skipped: { id: string; reason: string }[];
      }>('/api/mcp/servers/bulk-delete', {
        server_ids: Array.from(selectedIds),
      });
      if (res.skipped.length === 0) {
        toast.success(t('mcpServers.bulkDelete.success', { count: res.deleted.length }));
      } else {
        // Partial outcome: include both numbers + a short reason
        // breakdown so admins can tell whether a phantom id or a
        // permission issue triggered the skip.
        const reasons = res.skipped.map((s) => s.reason).join(', ');
        toast.warning(
          t('mcpServers.bulkDelete.partial', {
            deleted: res.deleted.length,
            skipped: res.skipped.length,
            reasons,
          }),
        );
      }
      setSelectedIds(new Set());
      setBulkDeleteOpen(false);
      await fetchServers();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
    } finally {
      setBulkDeleting(false);
    }
  };

  // Names of currently-selected servers, used in the confirm dialog
  // body. Falls back to display_label so the admin sees the same
  // string they're used to in the table.
  const selectedServerNames = servers
    .filter((s) => selectedIds.has(s.id))
    .map((s) => s.display_label ?? s.name);

  const handleDiscover = async (id: string) => {
    setDiscoveringId(id);
    try {
      const res = await apiPost<{
        status: 'discovery_complete' | 'auth_required' | 'discovery_failed';
        tools_discovered: number;
        error?: string;
      }>(`/api/mcp/servers/${id}/discover`, {});
      if (res.status === 'auth_required') {
        // Not an error — auth-required servers can't expose their
        // catalog to anonymous probes. Tools populate per user as
        // they connect via /connections. Use info toast (neutral),
        // not error (red).
        toast.info(t('mcpServers.discoverAuthRequired'));
      } else if (res.status === 'discovery_failed') {
        // Surface the underlying error string so admins can tell
        // whether it was a network timeout, a 5xx, a malformed
        // response, etc. The server bounded the length already.
        toast.error(t('mcpServers.discoverFailedDetail.title'), {
          description: res.error ?? t('mcpServers.discoverFailedDetail.unknown'),
        });
      } else {
        toast.success(
          t('mcpServers.discoverSuccess', { count: res.tools_discovered }),
        );
      }
      await fetchServers();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('mcpServers.discoverFailed'));
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
        {hasPermission('mcp_servers:create') ? (
          <Button asChild>
            <Link to="/mcp/servers/new">
              <Plus className="h-4 w-4" />
              {t('mcpServers.registerServer')}
            </Link>
          </Button>
        ) : (
          <Button disabled>
            <Plus className="h-4 w-4" />
            {t('mcpServers.registerServer')}
          </Button>
        )}
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {selectedIds.size > 0 && (
        <div className="flex items-center gap-2 mb-3">
          <span className="text-sm text-muted-foreground">
            {t('mcpServers.bulkDelete.selectedCount', { count: selectedIds.size })}
          </span>
          <Button
            variant="destructive"
            size="sm"
            onClick={() => setBulkDeleteOpen(true)}
            disabled={!hasPermission('mcp_servers:delete')}
          >
            <Trash2 className="mr-1 h-3.5 w-3.5" />
            {t('mcpServers.bulkDelete.action', { count: selectedIds.size })}
          </Button>
        </div>
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
                  <TableHead className="w-10">
                    <Checkbox
                      checked={headerCheckState}
                      onCheckedChange={toggleSelectAll}
                      aria-label={t('mcpServers.bulkDelete.selectAll')}
                      disabled={!hasPermission('mcp_servers:delete')}
                    />
                  </TableHead>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('mcpServers.endpointUrl')}</TableHead>
                  <TableHead className="w-20">{t('mcpServers.authMode')}</TableHead>
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
                  <TableRow key={s.id} data-state={selectedIds.has(s.id) ? 'selected' : undefined}>
                    <TableCell className="w-10">
                      <Checkbox
                        checked={selectedIds.has(s.id)}
                        onCheckedChange={() => toggleSelect(s.id)}
                        aria-label={t('mcpServers.bulkDelete.selectRow', { name: s.display_label ?? s.name })}
                        disabled={!hasPermission('mcp_servers:delete')}
                      />
                    </TableCell>
                    <TableCell className="font-medium">
                      {s.display_label ? (
                        // When the operator has set a display_label,
                        // show that prominently with the system name
                        // dimmed underneath — they're often different
                        // when two installs share a template.
                        <div className="flex flex-col">
                          <span>{s.display_label}</span>
                          <span className="text-xs font-normal text-muted-foreground">
                            {s.name}
                          </span>
                        </div>
                      ) : (
                        s.name
                      )}
                    </TableCell>
                    <TableCell className="font-mono text-xs">{s.endpoint_url}</TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <AuthModeBadge mode={deriveAuthMode(s)} compact />
                        {/* Anonymous servers carry no credential, so the
                            owner axis is meaningless — skip the badge. */}
                        {s.auth_shape !== 'anonymous' && (
                          <CredentialOwnerBadge owner={s.credential_owner} compact />
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      <TransportBadge transport={s.transport_type} />
                    </TableCell>
                    <TableCell>
                      {(() => {
                        // `auth_required` (set by `discover_and_persist_tools`
                        // when the upstream returns 401/403 to the anonymous
                        // probe) is the EXPECTED state for any non-anonymous
                        // server before a user has connected — it's not a
                        // degradation. Map it to `healthy` so admins don't
                        // chase a non-issue. The label still says "需要授权"
                        // so the actionable info isn't hidden.
                        const key: 'healthy' | 'down' | 'unknown' =
                          s.status === 'connected' || s.status === 'auth_required'
                            ? 'healthy'
                            : s.status === 'disconnected'
                              ? 'down'
                              : 'unknown';
                        // Inline so the i18n checker sees every key
                        // statically — `t(\`common.${dynamic}\`)` would
                        // require registering a DYNAMIC_ENUMS pattern.
                        const label =
                          s.status === 'auth_required'
                            ? t('common.authRequired')
                            : s.status === 'connected'
                              ? t('common.healthy')
                              : key === 'down'
                                ? t('common.down')
                                : t('common.unknown');
                        return (
                          <StatusIndicator
                            status={key}
                            label={label}
                            showLabel
                            pulse
                          />
                        );
                      })()}
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {s.last_health_check ? new Date(s.last_health_check).toLocaleString() : '—'}
                    </TableCell>
                    <TableCell className="text-sm">
                      {/* Auth-required servers don't have a system-level
                          tool catalog by design — `mcp_tools` is empty,
                          tools live per-user in `mcp_user_tools`. Show
                          em-dash so admins don't read "0 tools" as
                          "broken." */}
                      {s.status === 'auth_required' ? (
                        <span
                          className="text-muted-foreground"
                          title={t('mcpServers.toolsCountAuthRequiredHint')}
                        >
                          —
                        </span>
                      ) : (
                        s.tools_count
                      )}
                    </TableCell>
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
                          aria-label={t('common.edit')}
                          disabled={
                            !hasPermission('mcp_servers:update') ||
                            discoveringId === s.id
                          }
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
                          aria-label={t('mcpServers.discoverTools')}
                        >
                          {discoveringId === s.id ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() => setDeleteTargetId(s.id)}
                          title={t('common.delete')}
                          aria-label={t('common.delete')}
                          disabled={
                            !hasPermission('mcp_servers:delete') ||
                            discoveringId === s.id
                          }
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

      <ConfirmDialog
        open={bulkDeleteOpen}
        onOpenChange={setBulkDeleteOpen}
        title={t('mcpServers.bulkDelete.confirmTitle', { count: selectedIds.size })}
        // Include the names being removed in the body so the admin
        // can spot a misclick before confirming. Cap the listed names
        // at a sane length — the actual delete cap (50) is enforced
        // server-side too, but the text would get unwieldy past ~10.
        description={t('mcpServers.bulkDelete.confirmDescription', {
          count: selectedIds.size,
          names:
            selectedServerNames.length <= 10
              ? selectedServerNames.join(', ')
              : `${selectedServerNames.slice(0, 10).join(', ')}, …`,
        })}
        variant="destructive"
        confirmLabel={t('common.delete')}
        loading={bulkDeleting}
        onConfirm={() => void confirmBulkDelete()}
      />
    </div>
  );
}
