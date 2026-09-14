import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { keepPreviousData, skipToken, useQuery, useQueryClient } from '@tanstack/react-query';
import i18n from '@/i18n';
import { useResetOnChange } from '@/hooks/use-reset-on-change';
import { useSearch, useNavigate } from '@tanstack/react-router';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
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
import { AlertCircle, Brain, Plus, Trash2 } from 'lucide-react';
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
import { BatchImportDialog } from './BatchImportDialog';
import { ModelDetailSheet } from './ModelDetailSheet';
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

const NO_MODELS: ModelRow[] = [];
const NO_PROVIDERS: Provider[] = [];
const NO_ROUTE_HEALTH: Record<string, RouteHealthEntry> = {};

const routesKey = (modelId: string) => ['admin', 'models', modelId, 'routes'];

function healthByRoute(list: RouteHealthEntry[]): Record<string, RouteHealthEntry> {
  const m: Record<string, RouteHealthEntry> = {};
  for (const e of list) m[e.route_id] = e;
  return m;
}

/// The global default routing strategy, when the settings hold a valid one.
function defaultRoutingStrategy(data: {
  gateway?: { entries?: { key: string; value: unknown }[] };
}): RoutingStrategy | undefined {
  const v = data?.gateway?.entries?.find(
    (e) => e.key === 'gateway.default_routing_strategy',
  )?.value;
  if (
    typeof v === 'string' &&
    ['weighted', 'latency', 'health', 'latency_health'].includes(v)
  ) {
    return v as RoutingStrategy;
  }
  return undefined;
}

/* ---------- component ---------- */

export function ModelsPage() {
  const { t } = useTranslation();
  // Reads `?import=<providerId>` to auto-open the batch import dialog
  // on this provider — sent by the "Import Models" shortcut on the
  // Providers page. Typed via the route's `validateSearch`.
  const routeSearch = useSearch({ from: '/gateway/models' });
  const navigate = useNavigate();

  const queryClient = useQueryClient();

  // Model list state
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [statusFilter, setStatusFilter] = useState<'' | ModelStatus>('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const modelsQuery = useQuery({
    queryKey: [
      'admin',
      'models',
      { page, page_size: pageSize, q: debouncedSearch, status: statusFilter },
    ],
    queryFn: ({ signal }) => {
      const params = new URLSearchParams({ page: String(page), page_size: String(pageSize) });
      if (debouncedSearch) params.set('q', debouncedSearch);
      if (statusFilter) params.set('status', statusFilter);
      return api<{ items: ModelRow[]; total: number }>(`/api/admin/models?${params}`, {
        signal,
      });
    },
    // Keep the total — and so the pager and the cleanup count — steady
    // while another page loads. The rows still give way to the skeleton;
    // see `loading`.
    placeholderData: keepPreviousData,
  });
  const models = modelsQuery.data?.items ?? NO_MODELS;
  const totalModels = modelsQuery.data?.total ?? 0;
  const loading = modelsQuery.isPending || modelsQuery.isPlaceholderData;
  const error = modelsQuery.error?.message ?? '';
  /// Refetch the page on screen. Other cached pages are already stale
  /// and refetch when they are next shown.
  const invalidateModels = () =>
    queryClient.invalidateQueries({
      queryKey: [
        'admin',
        'models',
        { page, page_size: pageSize, q: debouncedSearch, status: statusFilter },
      ],
      exact: true,
    });

  // Providers — static lookup for the route editor + batch import.
  // Non-critical: the routes list still works without the lookup.
  const providersQuery = useQuery({
    queryKey: ['admin', 'providers'],
    queryFn: ({ signal }) => api<Provider[]>('/api/admin/providers', { signal }),
  });
  const providers = providersQuery.data ?? NO_PROVIDERS;

  // Platform baseline pricing — powers the "estimated cost" preview
  // shown inline under the weight fields. Non-critical — the cost
  // preview just won't render without it.
  const pricingQuery = useQuery({
    queryKey: ['admin', 'platform-pricing'],
    queryFn: ({ signal }) => api<PlatformPricing>('/api/admin/platform-pricing', { signal }),
  });
  const pricing = pricingQuery.data ?? null;

  // Delete-all-unrouted confirmation
  const [cleanupOpen, setCleanupOpen] = useState(false);
  const [cleanupRunning, setCleanupRunning] = useState(false);

  // Multi-select state for the bulk-delete action.
  // Stores model `id` (UUID), not `model_id`, since the bulk-delete
  // endpoint takes UUIDs to match the existing single-delete contract.
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false);
  const [bulkDeleting, setBulkDeleting] = useState(false);

  // Global default routing strategy. The wizard's mode picker uses it
  // to label "this model uses the global default" vs "this model
  // overrides". Falls back to `latency_health` so the UI keeps working
  // even if the settings endpoint is flaky.
  const settingsQuery = useQuery({
    queryKey: ['admin', 'settings'],
    queryFn: ({ signal }) =>
      api<{ gateway?: { entries?: { key: string; value: unknown }[] } }>('/api/admin/settings', {
        signal,
      }),
    select: defaultRoutingStrategy,
  });
  const globalStrategy: RoutingStrategy = settingsQuery.data ?? 'latency_health';

  // Detail drawer: which model_id is open, and its lazily-loaded routes.
  const [detailModelId, setDetailModelId] = useState<string | null>(null);
  const routesQuery = useQuery({
    queryKey: ['admin', 'models', detailModelId, 'routes'],
    queryFn: detailModelId
      ? ({ signal }) =>
          api<RouteRow[]>(`/api/admin/models/${encodeURIComponent(detailModelId)}/routes`, {
            signal,
          }).catch((err: unknown) => {
            if (!signal.aborted) {
              toast.error(err instanceof Error ? err.message : i18n.t('common.error'));
            }
            throw err;
          })
      : skipToken,
  });
  const invalidateRoutes = (modelId: string) =>
    queryClient.invalidateQueries({ queryKey: routesKey(modelId) });
  /// Per-route live health (state + EWMA latency), keyed by route_id.
  /// Only fetched while the detail drawer is open, and polled on a 5s
  /// cadence so the badge / latency column reflects what the breaker is
  /// actually using to make selection decisions: the tracker is the same
  /// Redis-backed view the breaker reads at selection time. Silent on
  /// failure — health is observability only.
  const routeHealthQuery = useQuery({
    queryKey: ['admin', 'models', detailModelId, 'route-health'],
    queryFn: detailModelId
      ? ({ signal }) =>
          api<RouteHealthEntry[]>(
            `/api/admin/models/${encodeURIComponent(detailModelId)}/route-health`,
            { signal },
          )
      : skipToken,
    select: healthByRoute,
    refetchInterval: 5000,
  });
  const routeHealth = routeHealthQuery.data ?? NO_ROUTE_HEALTH;

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

  // Deeplink handler: when landed with `?import=<providerId>` and the
  // provider list has finished loading, auto-open the batch dialog
  // pre-selected. Strip the param after firing so reopening the
  // dialog manually doesn't get re-triggered by a refresh.
  //
  // Opening the dialog follows from the URL and the provider list, so it
  // happens during render; only stripping the param is an effect. The
  // tracker starts empty rather than at the first value, because the
  // provider list can already be cached on the very first render.
  const importProviderId =
    routeSearch.import && providers.some((p) => p.id === routeSearch.import)
      ? routeSearch.import
      : null;
  const [seenImportProviderId, setSeenImportProviderId] = useState<string | null>(null);
  if (importProviderId !== seenImportProviderId) {
    setSeenImportProviderId(importProviderId);
    if (importProviderId) setBatchImport({ open: true, initialProviderId: importProviderId });
  }
  useEffect(() => {
    if (!importProviderId) return;
    void navigate({
      to: '/gateway/models',
      search: { import: undefined },
      replace: true,
    });
  }, [importProviderId, navigate]);

  useEffect(() => {
    const h = setTimeout(() => setDebouncedSearch(search.trim()), 250);
    return () => clearTimeout(h);
  }, [search]);

  useResetOnChange(debouncedSearch, () => setPage(1));

  // Drop selection whenever the visible page changes — selected IDs
  // could otherwise persist across pages where the user can no longer
  // see what they're about to delete.
  useResetOnChange(
    `${page}\u0000${pageSize}\u0000${debouncedSearch}\u0000${statusFilter}`,
    () => setSelectedIds(new Set()),
  );


  /* ---------- detail drawer ---------- */

  const openDetail = (modelId: string) => setDetailModelId(modelId);

  /* ---------- model CRUD ---------- */

  const openCreateModel = () => setModelEditor({ open: true, model: null });
  const openEditModel = (m: ModelRow) => setModelEditor({ open: true, model: m });

  const confirmCleanup = async () => {
    setCleanupRunning(true);
    try {
      const res = await apiDelete<{ deleted: number }>('/api/admin/models/unrouted');
      toast.success(t('models.cleanupDone', { count: res.deleted }));
      setCleanupOpen(false);
      await invalidateModels();
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
      await invalidateModels();
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
      await invalidateModels();
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
      await invalidateModels();
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
    const list = queryClient.getQueryData<RouteRow[]>(routesKey(modelId));
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
      await invalidateRoutes(modelId);
      await invalidateModels();
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
      await invalidateModels();
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
    // Cancel any refetch in flight first, so it can't land after the
    // optimistic write and put the old weights back.
    void queryClient.cancelQueries({ queryKey: routesKey(modelId) });
    const byId = new Map(updates.map((u) => [u.id, u.weight]));
    queryClient.setQueryData<RouteRow[]>(routesKey(modelId), (list) =>
      list?.map((r) => (byId.has(r.id) ? { ...r, weight: byId.get(r.id)! } : r)),
    );
    try {
      await apiPatch('/api/admin/model-routes/batch-weights', { updates });
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
      void invalidateRoutes(modelId);
    }
  };


  const confirmDeleteRoute = async () => {
    if (!deleteRoute) return;
    try {
      await apiDelete(`/api/admin/model-routes/${deleteRoute.id}`);
      toast.success(t('models.routeDeleted'));
      const modelId = deleteRoute.model_id;
      setDeleteRoute(null);
      await invalidateRoutes(modelId);
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

      {providersQuery.isFetched && providers.length === 0 && !loading && (
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
        onSaved={invalidateModels}
      />

      <RouteEditorDialog
        open={routeEditor.open}
        route={routeEditor.route}
        targetModel={routeEditor.targetModel}
        providers={providers}
        routeHealth={routeHealth}
        onClose={() => setRouteEditor({ open: false, route: null, targetModel: null })}
        onSaved={invalidateRoutes}
      />

      <BatchImportDialog
        open={batchImport.open}
        initialProviderId={batchImport.initialProviderId}
        providers={providers}
        detailModelId={detailModelId}
        onClose={() => setBatchImport({ open: false, initialProviderId: null })}
        onSaved={invalidateModels}
        onSavedForModel={invalidateRoutes}
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

      <ModelDetailSheet
        model={detailModelId ? models.find((m) => m.model_id === detailModelId) ?? null : null}
        routes={routesQuery.data}
        routesLoading={routesQuery.isLoading}
        routeHealth={routeHealth}
        globalStrategy={globalStrategy}
        pricing={pricing}
        providerLabel={providerLabel}
        onClose={() => setDetailModelId(null)}
        onEditModel={openEditModel}
        onDeleteModel={setDeleteModel}
        onAddRoute={openAddRoute}
        onEditRoute={openEditRoute}
        onDeleteRoute={setDeleteRoute}
        onSetAllRoutesEnabled={setAllRoutesEnabled}
        onUpdateModelStrategy={updateModelStrategy}
        onBatchUpdateWeights={batchUpdateWeights}
      />
    </div>
  );
}
