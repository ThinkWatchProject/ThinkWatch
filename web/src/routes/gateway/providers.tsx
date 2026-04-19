import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { StatusIndicator } from '@/components/ui/status-indicator';
import { ProviderTypeBadge } from '@/components/ui/provider-type-badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Plus, Trash2, Pencil, Plug, AlertCircle, Download } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, apiDelete, hasPermission } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';
import type { Provider } from './provider-types';
import { CreateProviderDialog } from './provider-dialogs';
import { EditProviderDialog } from './provider-dialogs';

export function ProvidersPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const pager = useClientPagination(providers, 20);
  const [error, setError] = useState('');
  const [createDialogOpen, setCreateDialogOpen] = useState(false);

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editProvider, setEditProvider] = useState<Provider | null>(null);
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  const fetchProviders = async (signal?: AbortSignal) => {
    try {
      const data = await api<Provider[]>('/api/admin/providers', { signal });
      setProviders(data);
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : 'Failed to load providers');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    const controller = new AbortController();
    fetchProviders(controller.signal);
    return () => controller.abort();
  }, []);

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
    setEditDialogOpen(true);
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('providers.title')}</h1>
          <p className="text-muted-foreground">{t('providers.subtitle')}</p>
        </div>
        <Button disabled={!hasPermission('providers:create')} onClick={() => setCreateDialogOpen(true)}>
          <Plus className="h-4 w-4" />
          {t('providers.addProvider')}
        </Button>
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
            <div className="flex h-full flex-col items-center justify-center text-center">
              <Plug className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('providers.noProviders')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('providers.noProvidersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
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
                {pager.paginated.map((p) => (
                  <TableRow key={p.id}>
                    <TableCell className="font-medium">{p.display_name || p.name}</TableCell>
                    <TableCell>
                      <ProviderTypeBadge type={p.provider_type} />
                    </TableCell>
                    <TableCell className="font-mono text-xs">{p.base_url}</TableCell>
                    <TableCell>
                      <StatusIndicator
                        status={p.is_active ? 'healthy' : 'inactive'}
                        label={p.is_active ? t('common.active') : t('common.inactive')}
                        showLabel
                      />
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(p.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          onClick={() =>
                            // Jump to Models page with `?import=<providerId>`;
                            // that page auto-opens the two-step import dialog
                            // pre-selected on this provider.
                            void navigate({
                              to: '/gateway/models',
                              search: { import: p.id },
                            })
                          }
                          title={t('providers.importModels')}
                          disabled={!hasPermission('models:write')}
                        >
                          <Download className="h-4 w-4" />
                        </Button>
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

      <CreateProviderDialog
        open={createDialogOpen}
        onOpenChange={setCreateDialogOpen}
        onSuccess={() => fetchProviders()}
      />

      <EditProviderDialog
        open={editDialogOpen}
        onOpenChange={setEditDialogOpen}
        provider={editProvider}
        onSuccess={() => fetchProviders()}
      />

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
