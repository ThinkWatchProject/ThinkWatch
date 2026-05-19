import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearch, useNavigate } from '@tanstack/react-router';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Checkbox } from '@/components/ui/checkbox';
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
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { LatencySparkline } from '../routing/LatencySparkline';
import { RoutingModeSection } from '../routing/RoutingModeSection';
import { TrafficBar } from '../routing/TrafficBar';
import { AlertCircle, Brain, Pencil, Plus, Trash2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api';
import { toast } from 'sonner';
import {
  AFFINITY_MODES,
  AUTO_TARGETS,
  ROUTING_STRATEGIES,
  modelStatus,
  type AffinityMode,
  type BreakerState,
  type ModelRow,
  type ModelStatus,
  type OutputGuardrail,
  type PlatformPricing,
  type RouteHealth,
  type RouteHealthEntry,
  type RouteHistoryBucket,
  type RouteHistoryResponse,
  type RouteRow,
  type RoutingStrategy,
} from './types';
import { ModelRowCell } from './ModelRowCell';
import { CostPreview } from './CostPreview';
import { BatchImportDialog } from './BatchImportDialog';
import { ModelEditorDialog } from './ModelEditorDialog';
import { RouteEditorDialog } from './RouteEditorDialog';
// Re-export the constants/types that other routes import from
// `'./models'` (e.g. RoutingModeSection consumes AUTO_TARGETS +
// RoutingStrategy). They live in `./models/types` now, but the
// public surface stays here so call sites don't have to update.
export {
  AFFINITY_MODES,
  AUTO_TARGETS,
  ROUTING_STRATEGIES,
};
export type {
  AffinityMode,
  BreakerState,
  ModelRow,
  OutputGuardrail,
  RouteHealth,
  RouteHealthEntry,
  RouteHistoryBucket,
  RouteHistoryResponse,
  RoutingStrategy,
};

// `Provider` reused from provider-types so models.tsx and providers.tsx
// can't drift apart. We previously declared a narrow local interface
// with only id/name/display_name/provider_type which silently ignored
// later additions to the canonical shape (e.g. region, config_json).
import type { Provider } from '../provider-types';

/* ---------- component ---------- */

export function ModelsPage() {
  const { t } = useTranslation();
  // Reads `?import=<providerId>` to auto-open the batch import dialog
  // on this provider — sent by the "Import Models" shortcut on the
  // Providers page. Typed via the route's `validateSearch`.
  const routeSearch = useSearch({ from: '/gateway/models' });
  const navigate = useNavigate();

  // Model list state
  const [models, setModels] = useState<ModelRow[]>([]);
  const [totalModels, setTotalModels] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [statusFilter, setStatusFilter] = useState<'' | ModelStatus>('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);

  // Providers — static lookup for the route editor + batch import.
  const [providers, setProviders] = useState<Provider[]>([]);

  // Platform baseline pricing — powers the "estimated cost" preview
  // shown inline under the weight fields. Loaded once on mount.
  const [pricing, setPricing] = useState<PlatformPricing | null>(null);

  // Delete-all-unrouted confirmation
  const [cleanupOpen, setCleanupOpen] = useState(false);
  const [cleanupRunning, setCleanupRunning] = useState(false);

  // Multi-select state for the bulk-delete action.
  // Stores model `id` (UUID), not `model_id`, since the bulk-delete
  // endpoint takes UUIDs to match the existing single-delete contract.
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false);
  const [bulkDeleting, setBulkDeleting] = useState(false);

  // Global default routing strategy. Fetched once when first detail
  // drawer opens, used by RoutingModeSection to show admin whether the
  // model's setting differs from the global default.
  const [globalStrategy, setGlobalStrategy] = useState<RoutingStrategy>('latency_health');

  // Detail drawer: which model_id is open, and its lazily-loaded routes.
  const [detailModelId, setDetailModelId] = useState<string | null>(null);
  const [routesByModel, setRoutesByModel] = useState<Record<string, RouteRow[]>>({});
  const [routesLoading, setRoutesLoading] = useState<Set<string>>(new Set());
  /// Per-route live health (state + EWMA latency), keyed by route_id.
  /// Only populated while the detail drawer is open and refreshed on a
  /// 5s interval so the badge / latency column reflects what the
  /// breaker is actually using to make selection decisions.
  const [routeHealth, setRouteHealth] = useState<Record<string, RouteHealthEntry>>({});

  // Model create/edit
  // Model edit/create — ModelEditorDialog owns form state + saving;
  // here we just track which mode it's in.
  const [modelEditor, setModelEditor] = useState<{ open: boolean; model: ModelRow | null }>({
    open: false,
    model: null,
  });
  const [deleteModel, setDeleteModel] = useState<ModelRow | null>(null);

  // Route create (one-off via "+ Add Provider") / edit — RouteEditorDialog
  // owns form state + remote-model fetch cache; we just pin which
  // route or target model the dialog is keyed to.
  const [routeEditor, setRouteEditor] = useState<{
    open: boolean;
    route: RouteRow | null;
    targetModel: ModelRow | null;
  }>({ open: false, route: null, targetModel: null });
  const [deleteRoute, setDeleteRoute] = useState<RouteRow | null>(null);

  // Batch import — BatchImportDialog owns its 2-step flow. We track
  // just open/initial-provider here so deeplink `?import=<pid>` can
  // pre-select.
  const [batchImport, setBatchImport] = useState<{
    open: boolean;
    initialProviderId: string | null;
  }>({ open: false, initialProviderId: null });

  /* ---------- data fetching ---------- */

  const fetchModels = useCallback(
    async (
      p = page,
      q = debouncedSearch,
      ps = pageSize,
      status: '' | ModelStatus = statusFilter,
    ) => {
      setLoading(true);
      try {
        const params = new URLSearchParams({ page: String(p), page_size: String(ps) });
        if (q) params.set('q', q);
        if (status) params.set('status', status);
        const res = await api<{ items: ModelRow[]; total: number }>(
          `/api/admin/models?${params}`,
        );
        setModels(res.items);
        setTotalModels(res.total);
        setError('');
      } catch (err) {
        setError(err instanceof Error ? err.message : t('common.error'));
      } finally {
        setLoading(false);
      }
    },
    [page, debouncedSearch, pageSize, statusFilter],
  );

  const fetchPricing = useCallback(async () => {
    try {
      const p = await api<PlatformPricing>('/api/admin/platform-pricing');
      setPricing(p);
    } catch {
      // Non-critical — cost preview just won't render.
    }
  }, []);

  const fetchProviders = useCallback(async () => {
    try {
      const provs = await api<Provider[]>('/api/admin/providers');
      setProviders(provs);
    } catch {
      // Non-critical: the routes list still works without the lookup.
    }
  }, []);

  const fetchRoutesFor = useCallback(async (modelId: string) => {
    setRoutesLoading((s) => new Set(s).add(modelId));
    try {
      const rows = await api<RouteRow[]>(
        `/api/admin/models/${encodeURIComponent(modelId)}/routes`,
      );
      setRoutesByModel((m) => ({ ...m, [modelId]: rows }));
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setRoutesLoading((s) => {
        const next = new Set(s);
        next.delete(modelId);
        return next;
      });
    }
  }, []);

  useEffect(() => {
    void fetchProviders();
    void fetchPricing();
    // Pull the global default strategy once. The wizard's mode picker
    // uses it to label "this model uses the global default" vs "this
    // model overrides". Falls back to `latency` on error so the UI
    // keeps working even if the settings endpoint is flaky.
    void api<{ gateway?: { entries?: { key: string; value: unknown }[] } }>(
      '/api/admin/settings',
    )
      .then((data) => {
        const v = data?.gateway?.entries?.find(
          (e) => e.key === 'gateway.default_routing_strategy',
        )?.value;
        if (
          typeof v === 'string' &&
          ['weighted', 'latency', 'health', 'latency_health'].includes(v)
        ) {
          setGlobalStrategy(v as RoutingStrategy);
        }
      })
      .catch(() => undefined);
  }, [fetchProviders, fetchPricing]);

  useEffect(() => {
    void fetchModels();
  }, [fetchModels]);

  // Deeplink handler: when landed with `?import=<providerId>` and the
  // provider list has finished loading, auto-open the batch dialog
  // pre-selected. Strip the param after firing so reopening the
  // dialog manually doesn't get re-triggered by a refresh.
  useEffect(() => {
    if (!routeSearch.import || providers.length === 0) return;
    const pid = routeSearch.import;
    if (!providers.some((p) => p.id === pid)) return;
    setBatchImport({ open: true, initialProviderId: pid });
    void navigate({
      to: '/gateway/models',
      search: { import: undefined },
      replace: true,
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [routeSearch.import, providers]);

  useEffect(() => {
    const h = setTimeout(() => setDebouncedSearch(search.trim()), 250);
    return () => clearTimeout(h);
  }, [search]);

  useEffect(() => {
    setPage(1);
  }, [debouncedSearch]);

  // Drop selection whenever the visible page changes — selected IDs
  // could otherwise persist across pages where the user can no longer
  // see what they're about to delete.
  useEffect(() => {
    setSelectedIds(new Set());
  }, [page, pageSize, debouncedSearch, statusFilter]);


  /* ---------- detail drawer ---------- */

  const openDetail = (modelId: string) => {
    setDetailModelId(modelId);
    if (!routesByModel[modelId]) void fetchRoutesFor(modelId);
  };

  // Poll route-health for the open model on a 5s cadence. The
  // tracker is the same Redis-backed view the breaker reads at
  // selection time, so the badges + EWMA column reflect what the
  // gateway is actually doing.
  useEffect(() => {
    if (!detailModelId) return;
    let cancelled = false;
    const fetchOnce = async () => {
      try {
        const list = await api<RouteHealthEntry[]>(
          `/api/admin/models/${encodeURIComponent(detailModelId)}/route-health`,
        );
        if (cancelled) return;
        const m: Record<string, RouteHealthEntry> = {};
        for (const e of list) m[e.route_id] = e;
        setRouteHealth(m);
      } catch {
        // Silent — health is observability only.
      }
    };
    void fetchOnce();
    const id = setInterval(() => {
      void fetchOnce();
    }, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [detailModelId]);

  /* ---------- model CRUD ---------- */

  const openCreateModel = () => setModelEditor({ open: true, model: null });
  const openEditModel = (m: ModelRow) => setModelEditor({ open: true, model: m });

  const confirmCleanup = async () => {
    setCleanupRunning(true);
    try {
      const res = await apiDelete<{ deleted: number }>('/api/admin/models/unrouted');
      toast.success(t('models.cleanupDone', { count: res.deleted }));
      setCleanupOpen(false);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setCleanupRunning(false);
    }
  };

  const bulkSetEnabled = async (enabled: boolean) => {
    if (selectedIds.size === 0) return;
    try {
      const res = await apiPost<{ updated: number }>(
        '/api/admin/models/bulk-set-enabled',
        { ids: Array.from(selectedIds), enabled },
      );
      toast.success(
        enabled
          ? t('models.batchEnabled', { count: res.updated })
          : t('models.batchDisabled', { count: res.updated }),
      );
      setSelectedIds(new Set());
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    }
  };

  const confirmBulkDelete = async () => {
    if (selectedIds.size === 0) return;
    setBulkDeleting(true);
    try {
      const res = await apiPost<{ deleted: number }>('/api/admin/models/bulk-delete', {
        ids: Array.from(selectedIds),
      });
      toast.success(t('models.bulkDeleted', { count: res.deleted }));
      setSelectedIds(new Set());
      setBulkDeleteOpen(false);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setBulkDeleting(false);
    }
  };

  const confirmDeleteModel = async () => {
    if (!deleteModel) return;
    try {
      await apiDelete(`/api/admin/models/${deleteModel.id}`);
      toast.success(t('models.toast.deleted'));
      setDeleteModel(null);
      setDetailModelId(null);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    }
  };

  /* ---------- route CRUD ---------- */

  const openAddRoute = (model: ModelRow) =>
    setRouteEditor({ open: true, route: null, targetModel: model });
  const openEditRoute = (route: RouteRow) =>
    setRouteEditor({ open: true, route, targetModel: null });

  /// Flip every route on a model on/off in one shot. Post-batch-import
  /// users land with a pile of `enabled = false` routes; this is how
  /// they go live without clicking each switch individually.
  const setAllRoutesEnabled = async (modelId: string, enabled: boolean) => {
    const list = routesByModel[modelId];
    if (!list || list.length === 0) return;
    const ids = list
      .filter((r) => r.enabled !== enabled)
      .map((r) => r.id);
    if (ids.length === 0) return;
    try {
      await apiPost('/api/admin/model-routes/batch-update', { ids, enabled });
      toast.success(
        enabled
          ? t('models.batchEnabled', { count: ids.length })
          : t('models.batchDisabled', { count: ids.length }),
      );
      await fetchRoutesFor(modelId);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    }
  };

  /// Persist the model's routing-strategy override. `null` clears the
  /// override (model falls back to global default).
  const updateModelStrategy = async (modelId: string, strategy: RoutingStrategy | null) => {
    const m = models.find((x) => x.model_id === modelId);
    if (!m) return;
    try {
      await apiPatch(`/api/admin/models/${m.id}`, { routing_strategy: strategy });
      // Refresh the model row so the cached `routing_strategy` field
      // reflects the new value next time the drawer opens.
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    }
  };

  /// Batch-update many route weights in one transaction. Used by the
  /// TrafficBar drag handle on release and the "even split" reset.
  ///
  /// Optimistic — we update `routesByModel` immediately so the UI
  /// reflects the dropped position with zero round-trip flicker, then
  /// fire the PATCH. Refetch only on failure to roll back to the
  /// authoritative state (rare; toast tells the admin what happened).
  const batchUpdateWeights = async (
    modelId: string,
    updates: { id: string; weight: number }[],
  ) => {
    if (updates.length === 0) return;
    setRoutesByModel((m) => {
      const list = m[modelId];
      if (!list) return m;
      const byId = new Map(updates.map((u) => [u.id, u.weight]));
      return {
        ...m,
        [modelId]: list.map((r) =>
          byId.has(r.id) ? { ...r, weight: byId.get(r.id)! } : r,
        ),
      };
    });
    try {
      await apiPatch('/api/admin/model-routes/batch-weights', { updates });
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
      void fetchRoutesFor(modelId);
    }
  };


  const confirmDeleteRoute = async () => {
    if (!deleteRoute) return;
    try {
      await apiDelete(`/api/admin/model-routes/${deleteRoute.id}`);
      toast.success(t('models.routeDeleted'));
      const modelId = deleteRoute.model_id;
      setDeleteRoute(null);
      await fetchRoutesFor(modelId);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    }
  };

  /* ---------- helpers ---------- */

  const providerLabel = (id: string): string => {
    const p = providers.find((p) => p.id === id);
    return p?.display_name || p?.name || id;
  };

  // Tri-state for the header checkbox: every visible row picked = `true`,
  // some picked = `'indeterminate'`, none picked = `false`.
  const allSelected = models.length > 0 && models.every((m) => selectedIds.has(m.id));
  const someSelected = !allSelected && models.some((m) => selectedIds.has(m.id));
  const headerCheckState: boolean | 'indeterminate' = allSelected
    ? true
    : someSelected
      ? 'indeterminate'
      : false;

  const toggleSelectAll = () => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (allSelected) for (const m of models) next.delete(m.id);
      else for (const m of models) next.add(m.id);
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

  /* ---------- render ---------- */

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Header */}
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('models.title')}</h1>
          <p className="text-muted-foreground">{t('models.subtitle')}</p>
        </div>
        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            onClick={() => setBatchImport({ open: true, initialProviderId: null })}
            disabled={providers.length === 0 || !hasPermission('models:write')}
          >
            <Plus className="mr-1 h-3.5 w-3.5" />
            {t('models.addRoutes')}
          </Button>
          <Button
            onClick={openCreateModel}
            disabled={!hasPermission('models:write')}
          >
            <Plus className="mr-1 h-3.5 w-3.5" />
            {t('models.addModel')}
          </Button>
        </div>
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

      {/* Search + status filter + cleanup action */}
      <div className="flex items-center gap-2 mb-4">
        <Input
          placeholder={t('models.searchPlaceholder')}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="max-w-sm"
        />
        <Select
          value={statusFilter || '__all__'}
          onValueChange={(v) => {
            setStatusFilter(v === '__all__' ? '' : (v as ModelStatus));
            setPage(1);
          }}
        >
          <SelectTrigger className="w-[170px]">
            <SelectValue placeholder={t('models.filterStatus')} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t('models.status.all')}</SelectItem>
            <SelectItem value="active">{t('models.status.active')}</SelectItem>
            <SelectItem value="disabled">{t('models.status.disabled')}</SelectItem>
            <SelectItem value="unrouted">{t('models.status.unrouted')}</SelectItem>
          </SelectContent>
        </Select>
        {selectedIds.size > 0 && (
          <div className="ml-auto flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => bulkSetEnabled(true)}
              disabled={!hasPermission('models:write')}
            >
              {t('models.bulkEnableAction', { count: selectedIds.size })}
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={() => bulkSetEnabled(false)}
              disabled={!hasPermission('models:write')}
            >
              {t('models.bulkDisableAction', { count: selectedIds.size })}
            </Button>
            <Button
              variant="destructive"
              size="sm"
              onClick={() => setBulkDeleteOpen(true)}
              disabled={!hasPermission('models:write')}
            >
              <Trash2 className="mr-1 h-3.5 w-3.5" />
              {t('models.bulkDeleteAction', { count: selectedIds.size })}
            </Button>
          </div>
        )}
        {selectedIds.size === 0 && statusFilter === 'unrouted' && totalModels > 0 && (
          <Button
            variant="outline"
            size="sm"
            className="ml-auto"
            onClick={() => setCleanupOpen(true)}
            disabled={!hasPermission('models:write')}
          >
            <Trash2 className="mr-1 h-3.5 w-3.5" />
            {t('models.cleanupAction', { count: totalModels })}
          </Button>
        )}
      </div>

      {/* Models table */}
      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        {/* Override the shared Table's `overflow-x-auto` wrapper to
            `overflow-visible` so the sticky `<thead>` resolves to
            this CardContent's scroll context — otherwise the wrapper
            acts as the sticky containing block and the header scrolls
            away with the body. */}
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-4 p-4">
              {[...Array(3)].map((_, i) => (
                <Skeleton key={i} className="h-12 w-full" />
              ))}
            </div>
          ) : models.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center text-center">
              <Brain className="mb-3 h-10 w-10 text-muted-foreground" />
              <p className="text-sm text-muted-foreground">{t('models.noModels')}</p>
              <p className="mt-1 text-xs text-muted-foreground">{t('models.noModelsHint')}</p>
            </div>
          ) : (
            <Table>
              {/* Sticky header keeps column labels visible while the body
                  scrolls inside the Card. `bg-card` matches the Card
                  surface so rows don't bleed through during scroll. */}
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead className="w-10">
                    <Checkbox
                      checked={headerCheckState}
                      onCheckedChange={toggleSelectAll}
                      aria-label={t('models.selectAll')}
                    />
                  </TableHead>
                  <TableHead>{t('models.col.modelId')}</TableHead>
                  <TableHead>{t('models.col.displayName')}</TableHead>
                  <TableHead className="text-center">{t('models.col.status')}</TableHead>
                  <TableHead className="text-right">{t('models.col.routeCount')}</TableHead>
                  <TableHead>{t('models.col.provider')}</TableHead>
                  <TableHead className="text-right">{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {models.map((m) => (
                  <ModelRowCell
                    key={m.id}
                    model={m}
                    selected={selectedIds.has(m.id)}
                    onToggleSelect={() => toggleSelect(m.id)}
                    onOpen={() => openDetail(m.model_id)}
                    onDelete={() => setDeleteModel(m)}
                  />
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        <div data-slot="card-footer" className="border-t">
          <DataTablePagination
            total={totalModels}
            page={page}
            pageSize={pageSize}
            onPageChange={setPage}
            onPageSizeChange={setPageSize}
          />
        </div>
      </Card>

      <ModelEditorDialog
        open={modelEditor.open}
        model={modelEditor.model}
        pricing={pricing}
        onClose={() => setModelEditor({ open: false, model: null })}
        onSaved={fetchModels}
      />

      <RouteEditorDialog
        open={routeEditor.open}
        route={routeEditor.route}
        targetModel={routeEditor.targetModel}
        providers={providers}
        routeHealth={routeHealth}
        onClose={() => setRouteEditor({ open: false, route: null, targetModel: null })}
        onSaved={fetchRoutesFor}
      />

      <BatchImportDialog
        open={batchImport.open}
        initialProviderId={batchImport.initialProviderId}
        providers={providers}
        detailModelId={detailModelId}
        onClose={() => setBatchImport({ open: false, initialProviderId: null })}
        onSaved={fetchModels}
        onSavedForModel={fetchRoutesFor}
      />

      {/* Delete confirms */}
      <ConfirmDialog
        open={deleteModel !== null}
        onOpenChange={(o) => {
          if (!o) setDeleteModel(null);
        }}
        title={t('models.deleteTitle')}
        description={t('models.deleteConfirm')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDeleteModel}
      />
      <ConfirmDialog
        open={deleteRoute !== null}
        onOpenChange={(o) => {
          if (!o) setDeleteRoute(null);
        }}
        title={t('models.deleteRouteTitle')}
        description={t('models.deleteRouteConfirm')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDeleteRoute}
      />
      <ConfirmDialog
        open={cleanupOpen}
        onOpenChange={setCleanupOpen}
        title={t('models.cleanupTitle')}
        description={t('models.cleanupConfirm', { count: totalModels })}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmCleanup}
        loading={cleanupRunning}
      />
      <ConfirmDialog
        open={bulkDeleteOpen}
        onOpenChange={setBulkDeleteOpen}
        title={t('models.bulkDeleteTitle')}
        description={t('models.bulkDeleteConfirm', { count: selectedIds.size })}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmBulkDelete}
        loading={bulkDeleting}
      />

      {/* Model detail drawer — right-side Sheet with basics + routes. */}
      <Sheet
        open={detailModelId !== null}
        onOpenChange={(o) => {
          if (!o) setDetailModelId(null);
        }}
      >
        <SheetContent className="w-full sm:max-w-xl overflow-y-auto">
          {detailModelId &&
            (() => {
              const model = models.find((m) => m.model_id === detailModelId);
              if (!model) return null;
              const routes = routesByModel[detailModelId];
              const rLoading = routesLoading.has(detailModelId);
              const status = modelStatus(model);
              return (
                <>
                  <SheetHeader>
                    <SheetTitle className="font-mono text-base break-all">
                      {model.model_id}
                    </SheetTitle>
                    <SheetDescription>
                      {model.display_name}
                      {' • '}
                      <span className="inline-block align-middle">
                        {status === 'active'
                          ? t('models.status.active')
                          : status === 'disabled'
                            ? t('models.status.disabled')
                            : t('models.status.unrouted')}
                      </span>
                    </SheetDescription>
                  </SheetHeader>
                  <div className="px-4 pb-4 space-y-6">
                    {/* Basics */}
                    <section className="space-y-2">
                      <div className="flex items-center justify-between">
                        <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                          {t('models.detail.basics')}
                        </Label>
                        <div className="flex items-center gap-1">
                          <Button
                            variant="outline"
                            size="sm"
                            className="h-7 text-xs"
                            onClick={() => openEditModel(model)}
                          >
                            <Pencil className="mr-1 h-3 w-3" />
                            {t('common.edit')}
                          </Button>
                          <Button
                            variant="outline"
                            size="sm"
                            className="h-7 text-xs text-destructive hover:text-destructive"
                            onClick={() => setDeleteModel(model)}
                          >
                            <Trash2 className="mr-1 h-3 w-3" />
                            {t('common.delete')}
                          </Button>
                        </div>
                      </div>
                      <div className="grid grid-cols-2 gap-2 text-xs">
                        <div>
                          <div className="text-muted-foreground">
                            {t('models.col.inputWeight')}
                          </div>
                          <div className="font-mono tabular-nums">{model.input_weight}</div>
                        </div>
                        <div>
                          <div className="text-muted-foreground">
                            {t('models.col.outputWeight')}
                          </div>
                          <div className="font-mono tabular-nums">{model.output_weight}</div>
                        </div>
                      </div>
                      <CostPreview
                        weight={model.input_weight}
                        basePerToken={pricing?.input_price_per_token}
                        currency={pricing?.currency}
                        side="input"
                      />
                      <CostPreview
                        weight={model.output_weight}
                        basePerToken={pricing?.output_price_per_token}
                        currency={pricing?.currency}
                        side="output"
                      />
                    </section>

                    {/* Routes */}
                    <section className="space-y-3">
                      {/* Strategy picker is meaningless when there's only one
                          enabled upstream — nothing to balance. The bar
                          inside the manual card uses the same gate. */}
                      {(routes ?? []).filter((r) => r.enabled).length > 1 && (
                        <RoutingModeSection
                          modelStrategy={
                            (model.routing_strategy as RoutingStrategy | null) ?? null
                          }
                          globalStrategy={globalStrategy}
                          disabled={!hasPermission('models:write')}
                          onChange={(next) => updateModelStrategy(model.model_id, next)}
                          manualBar={(() => {
                            const enabled = (routes ?? []).filter((r) => r.enabled);
                            if (enabled.length === 0) return null;
                            return (
                              <TrafficBar
                                segments={enabled.map((r) => ({
                                  id: r.id,
                                  label:
                                    r.label ??
                                    (r.upstream_model.split('/').pop() ?? r.upstream_model),
                                  weight: r.weight,
                                }))}
                                disabled={!hasPermission('models:write')}
                                onCommit={(updates) =>
                                  batchUpdateWeights(model.model_id, updates)
                                }
                              />
                            );
                          })()}
                        />
                      )}
                      <div className="flex items-center justify-between gap-2">
                        <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                          {t('models.routes')} ({routes?.length ?? model.route_count})
                        </Label>
                        <div className="flex items-center gap-1">
                          {/* Bulk enable/disable — most common post-import
                              action since batch-import creates routes
                              disabled by default. Shown only when there's
                              something in the opposite state to flip. */}
                          {routes && routes.some((r) => !r.enabled) && (
                            <Button
                              variant="outline"
                              size="sm"
                              className="h-7 text-xs"
                              onClick={() => setAllRoutesEnabled(model.model_id, true)}
                            >
                              {t('models.enableAllRoutes')}
                            </Button>
                          )}
                          {routes && routes.some((r) => r.enabled) && (
                            <Button
                              variant="outline"
                              size="sm"
                              className="h-7 text-xs"
                              onClick={() => setAllRoutesEnabled(model.model_id, false)}
                            >
                              {t('models.disableAllRoutes')}
                            </Button>
                          )}
                          <Button
                            variant="outline"
                            size="sm"
                            className="h-7 text-xs"
                            onClick={() => openAddRoute(model)}
                          >
                            <Plus className="mr-1 h-3 w-3" />
                            {t('models.addRoute')}
                          </Button>
                        </div>
                      </div>
                      {rLoading ? (
                        <Skeleton className="h-10 w-full" />
                      ) : !routes || routes.length === 0 ? (
                        <p className="text-xs italic text-muted-foreground py-2">
                          {t('models.noRoutes')}
                        </p>
                      ) : (
                        <div className="rounded-md border">
                          <table className="w-full text-xs">
                            <thead className="border-b bg-muted/30">
                              <tr className="text-left text-muted-foreground">
                                <th className="px-2 py-1.5 font-medium">
                                  {t('models.col.provider')}
                                </th>
                                <th className="px-2 py-1.5 font-medium">
                                  {t('models.col.upstreamModel')}
                                </th>
                                <th className="px-2 py-1.5 font-medium text-center">
                                  {t('models.col.active')}
                                </th>
                                <th
                                  className="px-2 py-1.5 font-medium text-center"
                                  title={t('models.col.healthHint')}
                                >
                                  {t('models.col.health')}
                                </th>
                                <th className="px-2 py-1.5 font-medium text-right">
                                  {t('models.col.p50')}
                                </th>
                                <th className="w-16" />
                              </tr>
                            </thead>
                            <tbody className="divide-y">
                              {routes.map((r) => {
                                  return (
                                    <tr key={r.id}>
                                      <td className="px-2 py-1.5">
                                        <div className="flex flex-col gap-0.5">
                                          <span>{providerLabel(r.provider_id)}</span>
                                          {r.label && (
                                            <span className="text-[10px] text-muted-foreground italic">
                                              {r.label}
                                            </span>
                                          )}
                                        </div>
                                      </td>
                                      <td
                                        className="px-2 py-1.5 font-mono text-[11px] max-w-[180px] truncate"
                                        title={r.upstream_model}
                                      >
                                        {r.upstream_model}
                                      </td>
                                      <td className="px-2 py-1.5 text-center">
                                        {r.enabled ? (
                                          <Badge variant="default" className="text-[10px]">
                                            {t('common.yes')}
                                          </Badge>
                                        ) : (
                                          <Badge variant="outline" className="text-[10px]">
                                            {t('common.no')}
                                          </Badge>
                                        )}
                                      </td>
                                      <td className="px-2 py-1.5 text-center">
                                        {(() => {
                                          const h = routeHealth[r.id]?.health;
                                          const state = h?.state ?? 'closed';
                                          const variant =
                                            state === 'closed'
                                              ? 'outline'
                                              : state === 'half_open'
                                                ? 'secondary'
                                                : 'destructive';
                                          return (
                                            <Badge variant={variant} className="text-[10px]">
                                              {t(`models.health.${state}`)}
                                            </Badge>
                                          );
                                        })()}
                                      </td>
                                      <td className="px-2 py-1.5 text-right font-mono text-[11px] tabular-nums">
                                        <div className="flex items-center justify-end gap-1.5">
                                          <LatencySparkline
                                            modelId={r.model_id}
                                            routeId={r.id}
                                          />
                                          {(() => {
                                            const ewma =
                                              routeHealth[r.id]?.health?.ewma_latency_ms;
                                            return ewma == null ? (
                                              <span className="text-muted-foreground">—</span>
                                            ) : (
                                              <span>{ewma.toFixed(0)} ms</span>
                                            );
                                          })()}
                                        </div>
                                      </td>
                                      <td className="px-2 py-1.5 text-right whitespace-nowrap">
                                        <Button
                                          variant="ghost"
                                          size="icon"
                                          className="h-7 w-7"
                                          onClick={() => openEditRoute(r)}
                                          aria-label={t('common.edit')}
                                          title={t('common.edit')}
                                        >
                                          <Pencil className="h-3.5 w-3.5" />
                                        </Button>
                                        <Button
                                          variant="ghost"
                                          size="icon"
                                          className="h-7 w-7"
                                          onClick={() => setDeleteRoute(r)}
                                          aria-label={t('common.delete')}
                                          title={t('common.delete')}
                                        >
                                          <Trash2 className="h-3.5 w-3.5 text-destructive" />
                                        </Button>
                                      </td>
                                    </tr>
                                  );
                                })}
                            </tbody>
                          </table>
                        </div>
                      )}
                    </section>
                  </div>
                </>
              );
            })()}
        </SheetContent>
      </Sheet>
    </div>
  );
}
