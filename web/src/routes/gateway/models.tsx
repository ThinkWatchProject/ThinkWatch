import { useCallback, useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { AlertCircle, Brain, Loader2, Pencil, Plus, Search, Trash2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { api, apiDelete, apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';

/* ---------- types ---------- */

interface RouteRow {
  id: string;
  model_id: string;
  provider_id: string;
  provider_name: string;
  upstream_model: string;
  weight: number;
  priority: number;
  enabled: boolean;
}

interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
}

/* ---------- form types ---------- */

interface RouteEditFormState {
  upstream_model: string;
  weight: string;
  priority: string;
  enabled: boolean;
}

/* ---------- component ---------- */

export function ModelsPage() {
  const { t } = useTranslation();

  // Route table state
  const [routes, setRoutes] = useState<RouteRow[]>([]);
  const [totalRoutes, setTotalRoutes] = useState(0);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');
  // `debouncedSearch` feeds the API so fast typing doesn't fan out
  // one request per keystroke.
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [filterProviderId, setFilterProviderId] = useState('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);

  // Edit route dialog
  const [editRouteDialogOpen, setEditRouteDialogOpen] = useState(false);
  const [editingRoute, setEditingRoute] = useState<RouteRow | null>(null);
  const [routeEditForm, setRouteEditForm] = useState<RouteEditFormState>({
    upstream_model: '',
    weight: '100',
    priority: '0',
    enabled: true,
  });
  const [routeEditFormError, setRouteEditFormError] = useState('');
  const [routeEditSaving, setRouteEditSaving] = useState(false);

  // Delete route
  const [deleteRouteId, setDeleteRouteId] = useState<string | null>(null);

  // Batch add dialog
  const [batchDialogOpen, setBatchDialogOpen] = useState(false);
  const [batchProviderId, setBatchProviderId] = useState('');
  const [remoteModels, setRemoteModels] = useState<string[]>([]);
  const [remoteModelsLoading, setRemoteModelsLoading] = useState(false);
  const [remoteModelsError, setRemoteModelsError] = useState('');
  const [batchSelected, setBatchSelected] = useState<Set<string>>(new Set());
  const [batchSearch, setBatchSearch] = useState('');
  const [batchSaving, setBatchSaving] = useState(false);
  // Track existing routes for the selected provider
  const [existingModelIds, setExistingModelIds] = useState<Set<string>>(new Set());

  /* ---------- data fetching ---------- */

  const fetchRoutes = useCallback(
    async (p = page, q = debouncedSearch, ps = pageSize, pid = filterProviderId) => {
      setLoading(true);
      try {
        const params = new URLSearchParams({ page: String(p), page_size: String(ps) });
        if (q) params.set('q', q);
        if (pid) params.set('provider_id', pid);
        const res = await api<{ items: RouteRow[]; total: number }>(
          `/api/admin/model-routes?${params}`,
        );
        setRoutes(res.items);
        setTotalRoutes(res.total);
        setError('');
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load routes');
      } finally {
        setLoading(false);
      }
    },
    [page, debouncedSearch, pageSize, filterProviderId],
  );

  // Providers list is small and only used by the filter dropdown +
  // add-route picker — fetch once on mount, not on every keystroke.
  const fetchProviders = useCallback(async () => {
    try {
      const provs = await api<Provider[]>('/api/admin/providers');
      setProviders(provs);
    } catch {
      // Route fetch surfaces its own error; providers are non-critical here.
    }
  }, []);

  useEffect(() => {
    void fetchProviders();
  }, [fetchProviders]);

  useEffect(() => {
    void fetchRoutes();
  }, [fetchRoutes]);

  // Debounce search input. 250ms matches the users page.
  useEffect(() => {
    const h = setTimeout(() => setDebouncedSearch(search.trim()), 250);
    return () => clearTimeout(h);
  }, [search]);

  // Reset to page 1 when the search term actually changes (post-debounce),
  // so you don't land on an empty page after filtering.
  useEffect(() => {
    setPage(1);
  }, [debouncedSearch]);

  /* ---------- edit route ---------- */

  const openEditRoute = (route: RouteRow) => {
    setEditingRoute(route);
    setRouteEditForm({
      upstream_model: route.upstream_model ?? '',
      weight: String(route.weight),
      priority: String(route.priority),
      enabled: route.enabled,
    });
    setRouteEditFormError('');
    setEditRouteDialogOpen(true);
  };

  const submitEditRoute = async (e: FormEvent) => {
    e.preventDefault();
    if (!editingRoute) return;
    setRouteEditFormError('');
    const weight = Number(routeEditForm.weight);
    if (!Number.isFinite(weight) || weight < 0) {
      setRouteEditFormError(t('models.weight') + ' must be >= 0');
      return;
    }
    const body = {
      upstream_model: routeEditForm.upstream_model.trim() || null,
      weight,
      priority: Number(routeEditForm.priority),
      enabled: routeEditForm.enabled,
    };
    setRouteEditSaving(true);
    try {
      await apiPatch(`/api/admin/model-routes/${editingRoute.id}`, body);
      toast.success(t('models.toast.updated'));
      setEditRouteDialogOpen(false);
      await fetchRoutes();
    } catch (err) {
      setRouteEditFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setRouteEditSaving(false);
    }
  };

  /* ---------- delete route ---------- */

  const confirmDeleteRoute = async () => {
    if (!deleteRouteId) return;
    try {
      await apiDelete(`/api/admin/model-routes/${deleteRouteId}`);
      toast.success(t('models.routeDeleted'));
      setDeleteRouteId(null);
      await fetchRoutes();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete route');
    }
  };

  /* ---------- batch add ---------- */

  const openBatchDialog = () => {
    setBatchProviderId('');
    setRemoteModels([]);
    setRemoteModelsError('');
    setBatchSelected(new Set());
    setBatchSearch('');
    setExistingModelIds(new Set());
    setBatchDialogOpen(true);
  };

  // Fetch remote models when provider changes in batch dialog
  const onBatchProviderChange = async (providerId: string) => {
    setBatchProviderId(providerId);
    setBatchSelected(new Set());
    setBatchSearch('');
    setRemoteModels([]);
    setRemoteModelsError('');
    setExistingModelIds(new Set());

    if (!providerId) return;

    setRemoteModelsLoading(true);
    try {
      // Fetch remote models and existing routes for this provider in parallel
      const [models, existingRoutes] = await Promise.all([
        api<string[]>(`/api/admin/providers/${providerId}/remote-models`),
        api<{ items: RouteRow[]; total: number }>(
          `/api/admin/model-routes?provider_id=${providerId}&page=1&page_size=10000`,
        ),
      ]);
      setRemoteModels(models);
      setExistingModelIds(new Set(existingRoutes.items.map((r) => r.model_id)));
    } catch (err) {
      setRemoteModelsError(err instanceof Error ? err.message : 'Failed to load models');
    } finally {
      setRemoteModelsLoading(false);
    }
  };

  const filteredRemoteModels = useMemo(() => {
    if (!batchSearch) return remoteModels;
    const q = batchSearch.toLowerCase();
    return remoteModels.filter((m) => m.toLowerCase().includes(q));
  }, [remoteModels, batchSearch]);

  const toggleBatchModel = (modelId: string) => {
    if (existingModelIds.has(modelId)) return;
    setBatchSelected((prev) => {
      const next = new Set(prev);
      if (next.has(modelId)) next.delete(modelId);
      else next.add(modelId);
      return next;
    });
  };

  const toggleSelectAll = () => {
    const selectable = filteredRemoteModels.filter((m) => !existingModelIds.has(m));
    const allSelected = selectable.length > 0 && selectable.every((m) => batchSelected.has(m));
    if (allSelected) {
      setBatchSelected((prev) => {
        const next = new Set(prev);
        for (const m of selectable) next.delete(m);
        return next;
      });
    } else {
      setBatchSelected((prev) => {
        const next = new Set(prev);
        for (const m of selectable) next.add(m);
        return next;
      });
    }
  };

  const submitBatch = async () => {
    if (!batchProviderId || batchSelected.size === 0) return;
    setBatchSaving(true);
    try {
      const res = await apiPost<{ created: number }>('/api/admin/model-routes/batch', {
        provider_id: batchProviderId,
        model_ids: Array.from(batchSelected),
      });
      toast.success(t('models.batchSuccess', { count: res.created }));
      setBatchDialogOpen(false);
      await fetchRoutes();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to add routes');
    } finally {
      setBatchSaving(false);
    }
  };

  /* ---------- render ---------- */

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Header */}
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('models.routeTitle')}</h1>
          <p className="text-muted-foreground">{t('models.routeSubtitle')}</p>
        </div>
        <Button onClick={openBatchDialog} disabled={providers.length === 0}>
          <Plus className="mr-1 h-3.5 w-3.5" />
          {t('models.addRoutes')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {providers.length === 0 && !loading && (
        <Alert className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{t('models.noProvidersHint')}</AlertDescription>
        </Alert>
      )}

      {/* Search + Provider filter */}
      <div className="flex items-center gap-2 mb-4">
        <Input
          placeholder={t('models.searchPlaceholder')}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="max-w-sm"
        />
        <Select
          value={filterProviderId}
          onValueChange={(v) => {
            setFilterProviderId(v === '__all__' ? '' : v);
            setPage(1);
          }}
        >
          <SelectTrigger className="w-[200px]">
            <SelectValue placeholder={t('models.filterProvider')} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t('models.filterProvider')}</SelectItem>
            {providers.map((p) => (
              <SelectItem key={p.id} value={p.id}>
                {p.display_name || p.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Route table */}
      {loading ? (
        <div className="space-y-4">
          {[...Array(3)].map((_, i) => (
            <Skeleton key={i} className="h-12 w-full" />
          ))}
        </div>
      ) : routes.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Brain className="mb-3 h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('models.noRoutes')}</p>
          </CardContent>
        </Card>
      ) : (
        <Card className="flex flex-col min-h-0 flex-1">
          <CardContent className="p-0 overflow-auto flex-1">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('models.col.modelId')}</TableHead>
                  <TableHead>{t('models.col.provider')}</TableHead>
                  <TableHead>{t('models.col.upstreamModel')}</TableHead>
                  <TableHead className="text-right">{t('models.col.weight')}</TableHead>
                  <TableHead>{t('models.col.priority')}</TableHead>
                  <TableHead className="text-center">{t('models.col.active')}</TableHead>
                  <TableHead className="text-right">{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {routes.map((r) => (
                  <TableRow key={r.id}>
                    <TableCell
                      className="font-mono text-xs max-w-[220px] truncate"
                      title={r.model_id}
                    >
                      {r.model_id}
                    </TableCell>
                    <TableCell className="text-sm">{r.provider_name}</TableCell>
                    <TableCell
                      className="font-mono text-xs max-w-[200px] truncate"
                      title={r.upstream_model || undefined}
                    >
                      {r.upstream_model || '\u2014'}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs">{r.weight}</TableCell>
                    <TableCell>
                      <Badge variant={r.priority === 0 ? 'default' : 'secondary'}>
                        {r.priority === 0 ? t('models.primary') : t('models.fallback')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-center">
                      {r.enabled ? (
                        <Badge variant="default">{t('common.yes')}</Badge>
                      ) : (
                        <Badge variant="outline">{t('common.no')}</Badge>
                      )}
                    </TableCell>
                    <TableCell className="text-right">
                      <Button variant="ghost" size="icon" onClick={() => openEditRoute(r)}>
                        <Pencil className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => setDeleteRouteId(r.id)}
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent>
          <div data-slot="card-footer" className="-mt-4 border-t">
            <DataTablePagination
              total={totalRoutes}
              page={page}
              pageSize={pageSize}
              onPageChange={setPage}
              onPageSizeChange={setPageSize}
            />
          </div>
        </Card>
      )}

      {/* Edit Route Dialog */}
      <Dialog open={editRouteDialogOpen} onOpenChange={setEditRouteDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitEditRoute}>
            <DialogHeader>
              <DialogTitle>{t('models.editTitle')}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="edit_upstream">{t('models.col.upstreamModel')}</Label>
                <Input
                  id="edit_upstream"
                  value={routeEditForm.upstream_model}
                  onChange={(e) =>
                    setRouteEditForm({ ...routeEditForm, upstream_model: e.target.value })
                  }
                  placeholder={t('models.upstreamModelHint')}
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="edit_weight">{t('models.col.weight')}</Label>
                  <Input
                    id="edit_weight"
                    value={routeEditForm.weight}
                    onChange={(e) =>
                      setRouteEditForm({ ...routeEditForm, weight: e.target.value })
                    }
                    inputMode="numeric"
                    required
                  />
                </div>
                <div className="space-y-2">
                  <Label>{t('models.col.priority')}</Label>
                  <Select
                    value={routeEditForm.priority}
                    onValueChange={(v) => setRouteEditForm({ ...routeEditForm, priority: v })}
                  >
                    <SelectTrigger>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="0">{t('models.primary')}</SelectItem>
                      <SelectItem value="1">{t('models.fallback')}</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
              </div>
              <div className="flex items-center gap-2">
                <Switch
                  id="route_enabled"
                  checked={routeEditForm.enabled}
                  onCheckedChange={(v) => setRouteEditForm({ ...routeEditForm, enabled: v })}
                />
                <Label htmlFor="route_enabled">{t('models.field.active')}</Label>
              </div>
              {routeEditFormError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{routeEditFormError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => setEditRouteDialogOpen(false)}
              >
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={routeEditSaving}>
                {routeEditSaving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Batch Add Routes Dialog */}
      <Dialog open={batchDialogOpen} onOpenChange={setBatchDialogOpen}>
        <DialogContent className="sm:max-w-lg max-h-[80vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>{t('models.addRoutes')}</DialogTitle>
            <DialogDescription>{t('models.routeSubtitle')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4 py-2 flex flex-col min-h-0 flex-1">
            {/* Provider select */}
            <div className="space-y-2">
              <Label>{t('models.selectProvider')}</Label>
              <Select value={batchProviderId} onValueChange={onBatchProviderChange}>
                <SelectTrigger>
                  <SelectValue placeholder={t('models.selectProvider')} />
                </SelectTrigger>
                <SelectContent>
                  {providers.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.display_name || p.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* Loading / error state */}
            {remoteModelsLoading && (
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                {t('models.loadingModels')}
              </div>
            )}

            {remoteModelsError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{remoteModelsError}</AlertDescription>
              </Alert>
            )}

            {/* Model list */}
            {!remoteModelsLoading && remoteModels.length > 0 && (
              <>
                <div className="flex items-center gap-2">
                  <div className="relative flex-1">
                    <Search className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                    <Input
                      placeholder={t('models.searchPlaceholder')}
                      value={batchSearch}
                      onChange={(e) => setBatchSearch(e.target.value)}
                      className="pl-9"
                    />
                  </div>
                  <span className="text-sm text-muted-foreground whitespace-nowrap">
                    {t('models.selected', { count: batchSelected.size })}
                  </span>
                  <Button type="button" variant="outline" size="sm" onClick={toggleSelectAll}>
                    {filteredRemoteModels.filter((m) => !existingModelIds.has(m)).length > 0 &&
                    filteredRemoteModels
                      .filter((m) => !existingModelIds.has(m))
                      .every((m) => batchSelected.has(m))
                      ? t('models.deselectAll')
                      : t('models.selectAll')}
                  </Button>
                </div>
                <div className="border rounded-md overflow-auto flex-1 min-h-0 max-h-[40vh]">
                  {filteredRemoteModels.map((modelId) => {
                    const exists = existingModelIds.has(modelId);
                    const checked = exists || batchSelected.has(modelId);
                    return (
                      <label
                        key={modelId}
                        className="flex items-center gap-2 px-3 py-1.5 hover:bg-muted/50 cursor-pointer text-sm border-b last:border-b-0"
                      >
                        <Checkbox
                          checked={checked}
                          disabled={exists}
                          onCheckedChange={() => toggleBatchModel(modelId)}
                        />
                        <span className={`font-mono text-xs truncate ${exists ? 'text-muted-foreground' : ''}`}>
                          {modelId}
                        </span>
                        {exists && (
                          <span className="text-xs text-muted-foreground ml-auto whitespace-nowrap">
                            ({t('models.alreadyExists')})
                          </span>
                        )}
                      </label>
                    );
                  })}
                </div>
              </>
            )}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setBatchDialogOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button
              type="button"
              disabled={batchSaving || batchSelected.size === 0}
              onClick={submitBatch}
            >
              {batchSaving ? (
                <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" />
              ) : null}
              {t('models.addNRoutes', { count: batchSelected.size })}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Delete Route Confirm */}
      <ConfirmDialog
        open={deleteRouteId !== null}
        onOpenChange={(o) => {
          if (!o) setDeleteRouteId(null);
        }}
        title={t('models.deleteTitle')}
        description={t('models.deleteConfirm')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDeleteRoute}
      />
    </div>
  );
}
