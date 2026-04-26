import { useCallback, useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearch, useNavigate } from '@tanstack/react-router';
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
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import {
  AlertCircle,
  Brain,
  ChevronDown,
  ChevronUp,
  Loader2,
  Pencil,
  Plus,
  Search,
  Trash2,
} from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api';
import { toast } from 'sonner';

/* ---------- types ---------- */

// Decimal fields come back from sqlx as strings (rust_decimal's default
// Serialize) — keep them that way in TS so we don't lose precision on
// parse, and let the form work in string space too.
interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
  /// Relative input-token cost factor. Absolute USD cost is
  /// `platform_pricing.input_price_per_token × input_weight × tokens`.
  input_weight: string;
  output_weight: string;
  route_count: number;
  enabled_route_count: number;
  /// Provider display names attached to this model, in priority order.
  /// Joined server-side so the row can render the column without
  /// fetching per-row routes.
  providers: string[];
}

type ModelStatus = 'active' | 'disabled' | 'unrouted';

function modelStatus(m: ModelRow): ModelStatus {
  if (m.route_count === 0) return 'unrouted';
  if (m.enabled_route_count === 0) return 'disabled';
  return 'active';
}

interface PlatformPricing {
  input_price_per_token: string;
  output_price_per_token: string;
  currency: string;
}

interface RouteRow {
  id: string;
  model_id: string;
  provider_id: string;
  provider_name: string;
  upstream_model: string | null;
  weight: number;
  priority: number;
  enabled: boolean;
}

// `Provider` reused from provider-types so models.tsx and providers.tsx
// can't drift apart. We previously declared a narrow local interface
// with only id/name/display_name/provider_type which silently ignored
// later additions to the canonical shape (e.g. region, config_json).
import type { Provider } from './provider-types';

interface ModelFormState {
  model_id: string;
  display_name: string;
  input_weight: string;
  output_weight: string;
}

interface RouteFormState {
  provider_id: string;
  upstream_model: string;
  weight: string;
  priority: string;
  enabled: boolean;
}

const emptyModelForm: ModelFormState = {
  model_id: '',
  display_name: '',
  input_weight: '1.0',
  output_weight: '1.0',
};

const emptyRouteForm: RouteFormState = {
  provider_id: '',
  upstream_model: '',
  weight: '100',
  priority: '0',
  enabled: true,
};

/* ---------- component ---------- */

export function ModelsPage() {
  const { t } = useTranslation();
  // Reads `?import=<providerId>` to auto-open the batch import dialog
  // on this provider — sent by the "Import Models" shortcut on the
  // Providers page.
  const routeSearch = useSearch({ strict: false }) as { import?: string };
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

  // Detail drawer: which model_id is open, and its lazily-loaded routes.
  const [detailModelId, setDetailModelId] = useState<string | null>(null);
  const [routesByModel, setRoutesByModel] = useState<Record<string, RouteRow[]>>({});
  const [routesLoading, setRoutesLoading] = useState<Set<string>>(new Set());

  // Model create/edit
  const [modelDialogOpen, setModelDialogOpen] = useState(false);
  const [editingModel, setEditingModel] = useState<ModelRow | null>(null);
  const [modelForm, setModelForm] = useState<ModelFormState>(emptyModelForm);
  const [modelFormError, setModelFormError] = useState('');
  const [modelSaving, setModelSaving] = useState(false);
  const [deleteModel, setDeleteModel] = useState<ModelRow | null>(null);

  // Route create (one-off via "+ Add Provider") / edit
  const [routeDialogOpen, setRouteDialogOpen] = useState(false);
  const [routeTargetModel, setRouteTargetModel] = useState<ModelRow | null>(null);
  const [editingRoute, setEditingRoute] = useState<RouteRow | null>(null);
  const [routeForm, setRouteForm] = useState<RouteFormState>(emptyRouteForm);
  const [routeFormError, setRouteFormError] = useState('');
  const [routeSaving, setRouteSaving] = useState(false);
  const [deleteRoute, setDeleteRoute] = useState<RouteRow | null>(null);

  // Per-provider remote-model list, cached for the route dialog's
  // upstream-model picker. `null` = not yet fetched, `[]` = fetched
  // empty (provider has no /models endpoint, fall back to free input).
  const [routeRemoteCache, setRouteRemoteCache] = useState<Record<string, string[] | null>>({});
  const [routeRemoteLoading, setRouteRemoteLoading] = useState(false);
  const [routeRemoteError, setRouteRemoteError] = useState('');

  // Batch import — two-step dialog.
  //
  // Step 1: pick a provider, tick remote models from its catalog.
  // Step 2: for each ticked model decide "new catalog entry" vs
  //         "attach as route to an existing exposed model".
  const [batchDialogOpen, setBatchDialogOpen] = useState(false);
  const [batchStep, setBatchStep] = useState<1 | 2>(1);
  const [batchProviderId, setBatchProviderId] = useState('');
  const [remoteModels, setRemoteModels] = useState<string[]>([]);
  const [remoteModelsLoading, setRemoteModelsLoading] = useState(false);
  const [remoteModelsError, setRemoteModelsError] = useState('');
  const [batchSelected, setBatchSelected] = useState<Set<string>>(new Set());
  const [batchSearch, setBatchSearch] = useState('');
  const [batchSaving, setBatchSaving] = useState(false);
  const [existingModelIds, setExistingModelIds] = useState<Set<string>>(new Set());
  // Picker source for the "attach" mode in step 2. Fetched once per
  // dialog open from /api/admin/models/ids.
  const [catalogModels, setCatalogModels] = useState<{ model_id: string; display_name: string }[]>(
    [],
  );
  // Per-upstream decisions made in step 2. Key = upstream name.
  type ImportDecision = { target_model_id: string | null; priority: number };
  const [batchDecisions, setBatchDecisions] = useState<Record<string, ImportDecision>>({});

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
        setError(err instanceof Error ? err.message : 'Failed to load models');
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
      toast.error(err instanceof Error ? err.message : 'Failed to load routes');
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
  }, [fetchProviders, fetchPricing]);

  useEffect(() => {
    void fetchModels();
  }, [fetchModels]);

  // Deeplink handler: when landed with `?import=<providerId>` and the
  // provider list has finished loading, auto-open the batch dialog
  // pre-selected. Strip the param after firing so reopening the dialog
  // manually doesn't get re-triggered by a refresh.
  useEffect(() => {
    if (!routeSearch.import || providers.length === 0) return;
    const pid = routeSearch.import;
    if (!providers.some((p) => p.id === pid)) return;
    openBatchDialog();
    void onBatchProviderChange(pid);
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

  // Pull the upstream-model picker options from the selected provider's
  // remote catalog. Cached per provider so reopening the dialog or
  // switching providers back-and-forth is instant.
  useEffect(() => {
    if (!routeDialogOpen) return;
    const pid = routeForm.provider_id;
    if (!pid) return;
    if (routeRemoteCache[pid] !== undefined) return;
    setRouteRemoteLoading(true);
    setRouteRemoteError('');
    void api<string[]>(`/api/admin/providers/${pid}/remote-models`)
      .then((rows) => {
        setRouteRemoteCache((c) => ({ ...c, [pid]: rows }));
      })
      .catch((err) => {
        // Provider with no /models endpoint, or temporary fetch
        // failure — leave cache empty and fall back to free input.
        setRouteRemoteCache((c) => ({ ...c, [pid]: null }));
        setRouteRemoteError(err instanceof Error ? err.message : 'Failed to load');
      })
      .finally(() => setRouteRemoteLoading(false));
  }, [routeDialogOpen, routeForm.provider_id, routeRemoteCache]);

  /* ---------- detail drawer ---------- */

  const openDetail = (modelId: string) => {
    setDetailModelId(modelId);
    if (!routesByModel[modelId]) void fetchRoutesFor(modelId);
  };

  /* ---------- model CRUD ---------- */

  const openCreateModel = () => {
    setEditingModel(null);
    setModelForm(emptyModelForm);
    setModelFormError('');
    setModelDialogOpen(true);
  };

  const openEditModel = (m: ModelRow) => {
    setEditingModel(m);
    setModelForm({
      model_id: m.model_id,
      display_name: m.display_name,
      input_weight: m.input_weight,
      output_weight: m.output_weight,
    });
    setModelFormError('');
    setModelDialogOpen(true);
  };

  const submitModel = async (e: FormEvent) => {
    e.preventDefault();
    setModelFormError('');
    const inW = Number(modelForm.input_weight);
    const outW = Number(modelForm.output_weight);
    if (!Number.isFinite(inW) || inW <= 0 || !Number.isFinite(outW) || outW <= 0) {
      setModelFormError(t('models.errors.weightMustBePositive'));
      return;
    }
    const body = {
      display_name: modelForm.display_name.trim() || modelForm.model_id.trim(),
      input_weight: inW,
      output_weight: outW,
    };
    setModelSaving(true);
    try {
      if (editingModel) {
        await apiPatch(`/api/admin/models/${editingModel.id}`, body);
        toast.success(t('models.toast.updated'));
      } else {
        if (!modelForm.model_id.trim()) {
          setModelFormError(t('models.field.modelId') + ' is required');
          setModelSaving(false);
          return;
        }
        await apiPost('/api/admin/models', {
          ...body,
          model_id: modelForm.model_id.trim(),
        });
        toast.success(t('models.toast.created'));
      }
      setModelDialogOpen(false);
      await fetchModels();
    } catch (err) {
      setModelFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setModelSaving(false);
    }
  };

  const confirmCleanup = async () => {
    setCleanupRunning(true);
    try {
      const res = await apiDelete<{ deleted: number }>('/api/admin/models/unrouted');
      toast.success(t('models.cleanupDone', { count: res.deleted }));
      setCleanupOpen(false);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Cleanup failed');
    } finally {
      setCleanupRunning(false);
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
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
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
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
    }
  };

  /* ---------- route CRUD ---------- */

  const openAddRoute = (model: ModelRow) => {
    setRouteTargetModel(model);
    setEditingRoute(null);
    setRouteForm(emptyRouteForm);
    setRouteFormError('');
    setRouteDialogOpen(true);
  };

  const openEditRoute = (route: RouteRow) => {
    setRouteTargetModel(null);
    setEditingRoute(route);
    setRouteForm({
      provider_id: route.provider_id,
      upstream_model: route.upstream_model ?? '',
      weight: String(route.weight),
      priority: String(route.priority),
      enabled: route.enabled,
    });
    setRouteFormError('');
    setRouteDialogOpen(true);
  };

  const submitRoute = async (e: FormEvent) => {
    e.preventDefault();
    setRouteFormError('');
    const weight = Number(routeForm.weight);
    if (!Number.isFinite(weight) || weight < 0) {
      setRouteFormError(t('models.weight') + ' must be >= 0');
      return;
    }
    const upstream = routeForm.upstream_model.trim() || null;
    setRouteSaving(true);
    try {
      if (editingRoute) {
        await apiPatch(`/api/admin/model-routes/${editingRoute.id}`, {
          upstream_model: upstream,
          weight,
          priority: Number(routeForm.priority),
          enabled: routeForm.enabled,
        });
        toast.success(t('models.toast.updated'));
        await fetchRoutesFor(editingRoute.model_id);
      } else if (routeTargetModel) {
        if (!routeForm.provider_id) {
          setRouteFormError(t('models.field.provider') + ' is required');
          setRouteSaving(false);
          return;
        }
        await apiPost(`/api/admin/models/${routeTargetModel.model_id}/routes`, {
          provider_id: routeForm.provider_id,
          upstream_model: upstream,
          weight,
          priority: Number(routeForm.priority),
          enabled: routeForm.enabled,
        });
        toast.success(t('models.routeAdded'));
        await fetchRoutesFor(routeTargetModel.model_id);
      }
      setRouteDialogOpen(false);
    } catch (err) {
      setRouteFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setRouteSaving(false);
    }
  };

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
      toast.error(err instanceof Error ? err.message : 'Failed');
    }
  };

  /// Move a route up/down in the priority list inside the drawer.
  ///
  /// Rules:
  ///   * Different priority than neighbor → swap priorities. Both
  ///     routes PATCH in parallel; one fetch afterwards refreshes
  ///     the cached list the drawer reads.
  ///   * Same priority (same load-balance bucket) → swap is a no-op,
  ///     so instead bump the moving route's priority by 1 in the
  ///     chosen direction. Clamped to 0.
  const moveRoute = async (modelId: string, index: number, dir: 'up' | 'down') => {
    const list = routesByModel[modelId];
    if (!list) return;
    const neighbor = dir === 'up' ? list[index - 1] : list[index + 1];
    if (!neighbor) return;
    const current = list[index];
    try {
      if (current.priority === neighbor.priority) {
        const newPri =
          dir === 'up' ? Math.max(0, current.priority - 1) : current.priority + 1;
        await apiPatch(`/api/admin/model-routes/${current.id}`, { priority: newPri });
      } else {
        await Promise.all([
          apiPatch(`/api/admin/model-routes/${current.id}`, { priority: neighbor.priority }),
          apiPatch(`/api/admin/model-routes/${neighbor.id}`, { priority: current.priority }),
        ]);
      }
      await fetchRoutesFor(modelId);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to reorder');
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
      toast.error(err instanceof Error ? err.message : 'Failed to delete route');
    }
  };

  /* ---------- batch import ---------- */

  const openBatchDialog = () => {
    setBatchStep(1);
    setBatchProviderId('');
    setRemoteModels([]);
    setRemoteModelsError('');
    setBatchSelected(new Set());
    setBatchSearch('');
    setExistingModelIds(new Set());
    setBatchDecisions({});
    setBatchDialogOpen(true);
    // Catalog lookup powers step 2's "attach to existing" picker.
    // Kicked off once per open so stepping back and forward is instant.
    void api<{ model_id: string; display_name: string }[]>('/api/admin/models/ids')
      .then(setCatalogModels)
      .catch(() => setCatalogModels([]));
  };

  /// Heuristic for "did the admin probably mean to attach this to an
  /// already-exposed model, or to make a new one?". Matches on exact
  /// name, else substring, else defaults to "new". Run once when we
  /// enter step 2 to pre-fill the decisions.
  const suggestDecision = (
    upstream: string,
    catalog: { model_id: string }[],
  ): ImportDecision => {
    const exact = catalog.find((c) => c.model_id === upstream);
    if (exact) return { target_model_id: exact.model_id, priority: 1 };
    const partial = catalog.find(
      (c) => upstream.includes(c.model_id) || c.model_id.includes(upstream),
    );
    if (partial) return { target_model_id: partial.model_id, priority: 1 };
    return { target_model_id: null, priority: 0 };
  };

  const goToStep2 = () => {
    // Pre-fill one decision per selected upstream using the heuristic.
    const next: Record<string, ImportDecision> = {};
    for (const u of batchSelected) next[u] = suggestDecision(u, catalogModels);
    setBatchDecisions(next);
    setBatchStep(2);
  };

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
      const [rmodels, existing] = await Promise.all([
        api<string[]>(`/api/admin/providers/${providerId}/remote-models`),
        api<{ items: RouteRow[]; total: number }>(
          `/api/admin/model-routes?provider_id=${providerId}&page=1&page_size=10000`,
        ),
      ]);
      setRemoteModels(rmodels);
      // A remote name counts as "already imported" when it appears as
      // either a route's exposed model_id (new-catalog-entry imports) or
      // its upstream_model (attach-to-existing imports — where model_id
      // is the rename target, so a model_id-only check would miss it).
      const seen = new Set<string>();
      for (const r of existing.items) {
        seen.add(r.model_id);
        if (r.upstream_model) seen.add(r.upstream_model);
      }
      setExistingModelIds(seen);
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

  const toggleBatchSelectAll = () => {
    const selectable = filteredRemoteModels.filter((m) => !existingModelIds.has(m));
    const allSelected = selectable.length > 0 && selectable.every((m) => batchSelected.has(m));
    setBatchSelected((prev) => {
      const next = new Set(prev);
      if (allSelected) for (const m of selectable) next.delete(m);
      else for (const m of selectable) next.add(m);
      return next;
    });
  };

  const submitBatch = async () => {
    if (!batchProviderId || batchSelected.size === 0) return;
    setBatchSaving(true);
    try {
      const items = Array.from(batchSelected).map((upstream) => {
        const d = batchDecisions[upstream] ?? { target_model_id: null, priority: 0 };
        return {
          upstream,
          target_model_id: d.target_model_id,
          priority: d.priority,
        };
      });
      const res = await apiPost<{ created: number }>('/api/admin/model-routes/batch', {
        provider_id: batchProviderId,
        items,
      });
      toast.success(t('models.batchSuccess', { count: res.created }));
      setBatchDialogOpen(false);
      await fetchModels();
      // If the drawer is open on a model we just touched, refresh it.
      if (detailModelId) void fetchRoutesFor(detailModelId);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to import');
    } finally {
      setBatchSaving(false);
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
            onClick={openBatchDialog}
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
          <Button
            variant="destructive"
            size="sm"
            className="ml-auto"
            onClick={() => setBulkDeleteOpen(true)}
            disabled={!hasPermission('models:write')}
          >
            <Trash2 className="mr-1 h-3.5 w-3.5" />
            {t('models.bulkDeleteAction', { count: selectedIds.size })}
          </Button>
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
                  <ModelRow
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

      {/* Create / Edit Model Dialog */}
      <Dialog open={modelDialogOpen} onOpenChange={setModelDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitModel}>
            <DialogHeader>
              <DialogTitle>
                {editingModel ? t('models.editTitle') : t('models.createTitle')}
              </DialogTitle>
              <DialogDescription>{t('models.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              {!editingModel && (
                <div className="space-y-2">
                  <Label htmlFor="model_id">{t('models.field.modelId')}</Label>
                  <Input
                    id="model_id"
                    value={modelForm.model_id}
                    onChange={(e) => setModelForm({ ...modelForm, model_id: e.target.value })}
                    placeholder="gpt-4o"
                    required
                  />
                </div>
              )}
              <div className="space-y-2">
                <Label htmlFor="model_display">{t('models.field.displayName')}</Label>
                <Input
                  id="model_display"
                  value={modelForm.display_name}
                  onChange={(e) => setModelForm({ ...modelForm, display_name: e.target.value })}
                  placeholder={modelForm.model_id}
                />
              </div>
              <p className="text-xs text-muted-foreground">{t('models.weightHint')}</p>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="input_weight">{t('models.field.inputWeight')}</Label>
                  <Input
                    id="input_weight"
                    value={modelForm.input_weight}
                    onChange={(e) =>
                      setModelForm({ ...modelForm, input_weight: e.target.value })
                    }
                    inputMode="decimal"
                    required
                  />
                  <CostPreview
                    weight={modelForm.input_weight}
                    basePerToken={pricing?.input_price_per_token}
                    currency={pricing?.currency}
                    side="input"
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="output_weight">{t('models.field.outputWeight')}</Label>
                  <Input
                    id="output_weight"
                    value={modelForm.output_weight}
                    onChange={(e) =>
                      setModelForm({ ...modelForm, output_weight: e.target.value })
                    }
                    inputMode="decimal"
                    required
                  />
                  <CostPreview
                    weight={modelForm.output_weight}
                    basePerToken={pricing?.output_price_per_token}
                    currency={pricing?.currency}
                    side="output"
                  />
                </div>
              </div>
              {modelFormError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{modelFormError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setModelDialogOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={modelSaving}>
                {modelSaving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Create / Edit Route Dialog */}
      <Dialog open={routeDialogOpen} onOpenChange={setRouteDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitRoute}>
            <DialogHeader>
              <DialogTitle>
                {editingRoute ? t('models.editRouteTitle') : t('models.addRouteTitle')}
              </DialogTitle>
              <DialogDescription>
                {editingRoute
                  ? editingRoute.model_id
                  : routeTargetModel?.model_id ?? ''}
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              {!editingRoute && (
                <div className="space-y-2">
                  <Label>{t('models.field.provider')}</Label>
                  <Select
                    value={routeForm.provider_id}
                    onValueChange={(v) => setRouteForm({ ...routeForm, provider_id: v })}
                  >
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
              )}
              <div className="space-y-2">
                <Label htmlFor="route_upstream">{t('models.col.upstreamModel')}</Label>
                {(() => {
                  const pid = routeForm.provider_id;
                  const remote = pid ? routeRemoteCache[pid] : undefined;
                  // Loading: provider picked, fetch in flight.
                  if (pid && remote === undefined && routeRemoteLoading) {
                    return (
                      <div className="flex items-center gap-2 text-xs text-muted-foreground h-9 px-3 border rounded-md">
                        <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        {t('models.loadingModels')}
                      </div>
                    );
                  }
                  // Fetched a usable list → searchable select with an
                  // explicit "(use catalog model_id)" option for the
                  // NULL-upstream case.
                  if (remote && remote.length > 0) {
                    const INHERIT = '__inherit__';
                    return (
                      <Select
                        value={routeForm.upstream_model || INHERIT}
                        onValueChange={(v) =>
                          setRouteForm({
                            ...routeForm,
                            upstream_model: v === INHERIT ? '' : v,
                          })
                        }
                      >
                        <SelectTrigger id="route_upstream">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value={INHERIT}>
                            <span className="italic text-muted-foreground">
                              {t('models.upstreamModelInherit', {
                                modelId:
                                  editingRoute?.model_id ?? routeTargetModel?.model_id ?? '',
                              })}
                            </span>
                          </SelectItem>
                          {remote.map((m) => (
                            <SelectItem key={m} value={m}>
                              <span className="font-mono text-xs">{m}</span>
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    );
                  }
                  // No provider chosen yet, or remote list unavailable
                  // — fall back to free input so the user is never
                  // blocked from saving a custom upstream name.
                  return (
                    <>
                      <Input
                        id="route_upstream"
                        value={routeForm.upstream_model}
                        onChange={(e) =>
                          setRouteForm({ ...routeForm, upstream_model: e.target.value })
                        }
                        placeholder={t('models.upstreamModelHint')}
                      />
                      {pid && routeRemoteError && (
                        <p className="text-[11px] text-muted-foreground">
                          {t('models.upstreamModelFreeInput')}
                        </p>
                      )}
                    </>
                  );
                })()}
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="route_weight">{t('models.col.weight')}</Label>
                  <Input
                    id="route_weight"
                    value={routeForm.weight}
                    onChange={(e) => setRouteForm({ ...routeForm, weight: e.target.value })}
                    inputMode="numeric"
                    required
                  />
                </div>
                <div className="space-y-2">
                  <Label>{t('models.col.priority')}</Label>
                  <Select
                    value={routeForm.priority}
                    onValueChange={(v) => setRouteForm({ ...routeForm, priority: v })}
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
                  checked={routeForm.enabled}
                  onCheckedChange={(v) => setRouteForm({ ...routeForm, enabled: v })}
                />
                <Label htmlFor="route_enabled">{t('models.field.active')}</Label>
              </div>
              {routeFormError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{routeFormError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setRouteDialogOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={routeSaving}>
                {routeSaving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Batch Import Dialog — two-step:
           1. Pick provider + tick remote models
           2. Decide per-item: new exposed model vs route on an existing one */}
      <Dialog open={batchDialogOpen} onOpenChange={setBatchDialogOpen}>
        <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>
              {t('models.addRoutes')}{' '}
              <span className="text-xs font-normal text-muted-foreground">
                {t('models.stepOf', { current: batchStep, total: 2 })}
              </span>
            </DialogTitle>
            <DialogDescription>
              {batchStep === 1
                ? t('models.batchImportHint')
                : t('models.batchStep2Hint')}
            </DialogDescription>
          </DialogHeader>

          {batchStep === 1 && (
            <div className="space-y-4 py-2 flex flex-col min-h-0 flex-1">
              <Alert>
                <AlertCircle className="h-4 w-4" />
                <AlertDescription className="text-xs">
                  {t('models.batchImportWarning')}
                </AlertDescription>
              </Alert>
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
                    <Button type="button" variant="outline" size="sm" onClick={toggleBatchSelectAll}>
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
                          <span
                            className={`font-mono text-xs truncate ${exists ? 'text-muted-foreground' : ''}`}
                          >
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
          )}

          {batchStep === 2 && (
            <div className="space-y-3 py-2 flex flex-col min-h-0 flex-1">
              <div className="flex items-center gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    const next = { ...batchDecisions };
                    for (const u of batchSelected) next[u] = { target_model_id: null, priority: 0 };
                    setBatchDecisions(next);
                  }}
                >
                  {t('models.batchAllNew')}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    const next = { ...batchDecisions };
                    for (const u of batchSelected) next[u] = suggestDecision(u, catalogModels);
                    setBatchDecisions(next);
                  }}
                >
                  {t('models.batchResetSuggestions')}
                </Button>
              </div>
              <div className="border rounded-md overflow-auto flex-1 min-h-0 max-h-[50vh] divide-y">
                {Array.from(batchSelected)
                  .sort()
                  .map((upstream) => {
                    const decision = batchDecisions[upstream] ?? {
                      target_model_id: null,
                      priority: 0,
                    };
                    const setDecision = (d: ImportDecision) =>
                      setBatchDecisions({ ...batchDecisions, [upstream]: d });
                    return (
                      <div key={upstream} className="p-3 space-y-2">
                        <div className="font-mono text-xs break-all">{upstream}</div>
                        <div className="flex items-center gap-2 text-xs">
                          <Select
                            value={decision.target_model_id ?? '__new__'}
                            onValueChange={(v) => {
                              if (v === '__new__') {
                                setDecision({ target_model_id: null, priority: 0 });
                              } else {
                                setDecision({
                                  target_model_id: v,
                                  priority: decision.priority === 0 ? 1 : decision.priority,
                                });
                              }
                            }}
                          >
                            <SelectTrigger className="h-7 text-xs flex-1">
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value="__new__">
                                {t('models.batchModeNew')}
                              </SelectItem>
                              {catalogModels.map((c) => (
                                <SelectItem key={c.model_id} value={c.model_id}>
                                  {t('models.batchModeAttach', { target: c.model_id })}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                          {decision.target_model_id && (
                            <Select
                              value={String(decision.priority)}
                              onValueChange={(v) =>
                                setDecision({ ...decision, priority: Number(v) })
                              }
                            >
                              <SelectTrigger className="h-7 text-xs w-28">
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value="0">{t('models.primary')}</SelectItem>
                                <SelectItem value="1">{t('models.fallback')}</SelectItem>
                              </SelectContent>
                            </Select>
                          )}
                        </div>
                      </div>
                    );
                  })}
              </div>
            </div>
          )}

          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setBatchDialogOpen(false)}>
              {t('common.cancel')}
            </Button>
            {batchStep === 1 ? (
              <Button
                type="button"
                disabled={batchSelected.size === 0}
                onClick={goToStep2}
              >
                {t('models.batchNextStep', { count: batchSelected.size })}
              </Button>
            ) : (
              <>
                <Button type="button" variant="outline" onClick={() => setBatchStep(1)}>
                  {t('common.previous')}
                </Button>
                <Button type="button" disabled={batchSaving} onClick={submitBatch}>
                  {batchSaving ? <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" /> : null}
                  {t('models.addNRoutes', { count: batchSelected.size })}
                </Button>
              </>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>

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

      {/* Model detail drawer — right-side Sheet with basics + routes + danger. */}
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
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 text-xs"
                          onClick={() => openEditModel(model)}
                        >
                          <Pencil className="mr-1 h-3 w-3" />
                          {t('common.edit')}
                        </Button>
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
                    <section className="space-y-2">
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
                                <th className="px-2 py-1.5 font-medium">
                                  {t('models.col.priority')}
                                </th>
                                <th className="px-2 py-1.5 font-medium text-center">
                                  {t('models.col.active')}
                                </th>
                                <th className="w-16" />
                              </tr>
                            </thead>
                            <tbody className="divide-y">
                              {routes.map((r, idx) => (
                                <tr key={r.id}>
                                  <td className="px-2 py-1.5">
                                    {providerLabel(r.provider_id)}
                                  </td>
                                  <td
                                    className="px-2 py-1.5 font-mono text-[11px] max-w-[180px] truncate"
                                    title={r.upstream_model ?? model.model_id}
                                  >
                                    {r.upstream_model ?? (
                                      <span className="italic text-muted-foreground">
                                        {model.model_id}
                                      </span>
                                    )}
                                  </td>
                                  <td className="px-2 py-1.5">
                                    <Badge
                                      variant={r.priority === 0 ? 'default' : 'secondary'}
                                      className="text-[10px]"
                                    >
                                      {r.priority === 0
                                        ? t('models.primary')
                                        : `${t('models.fallback')} (P${r.priority})`}
                                    </Badge>
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
                                  <td className="px-2 py-1.5 text-right whitespace-nowrap">
                                    <Button
                                      variant="ghost"
                                      size="icon"
                                      className="h-6 w-6"
                                      disabled={idx === 0}
                                      onClick={() => moveRoute(r.model_id, idx, 'up')}
                                      aria-label={t('models.moveUp')}
                                    >
                                      <ChevronUp className="h-3.5 w-3.5" />
                                    </Button>
                                    <Button
                                      variant="ghost"
                                      size="icon"
                                      className="h-6 w-6"
                                      disabled={idx === routes.length - 1}
                                      onClick={() => moveRoute(r.model_id, idx, 'down')}
                                      aria-label={t('models.moveDown')}
                                    >
                                      <ChevronDown className="h-3.5 w-3.5" />
                                    </Button>
                                    <Button
                                      variant="ghost"
                                      size="icon"
                                      className="h-7 w-7"
                                      onClick={() => openEditRoute(r)}
                                    >
                                      <Pencil className="h-3.5 w-3.5" />
                                    </Button>
                                    <Button
                                      variant="ghost"
                                      size="icon"
                                      className="h-7 w-7"
                                      onClick={() => setDeleteRoute(r)}
                                    >
                                      <Trash2 className="h-3.5 w-3.5 text-destructive" />
                                    </Button>
                                  </td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </div>
                      )}
                    </section>

                    {/* Danger zone */}
                    <section className="space-y-2 rounded-md border border-destructive/30 p-3">
                      <Label className="text-xs font-semibold uppercase tracking-wider text-destructive">
                        {t('models.detail.danger')}
                      </Label>
                      <p className="text-[11px] text-muted-foreground">
                        {t('models.deleteConfirm')}
                      </p>
                      <Button
                        variant="destructive"
                        size="sm"
                        onClick={() => {
                          setDetailModelId(null);
                          setDeleteModel(model);
                        }}
                      >
                        <Trash2 className="mr-1 h-3.5 w-3.5" />
                        {t('models.deleteTitle')}
                      </Button>
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

/* ---------- compact main-table row ---------- */

function ModelRow({
  model,
  selected,
  onToggleSelect,
  onOpen,
  onDelete,
}: {
  model: ModelRow;
  selected: boolean;
  onToggleSelect: () => void;
  onOpen: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const status = modelStatus(model);
  return (
    <TableRow
      className="cursor-pointer hover:bg-muted/30"
      data-state={selected ? 'selected' : undefined}
      onClick={(e) => {
        const target = e.target as HTMLElement;
        // Don't open the drawer when the user is interacting with row
        // controls (action buttons or the select checkbox).
        if (target.closest('button')) return;
        if (target.closest('[role="checkbox"]')) return;
        onOpen();
      }}
    >
      <TableCell
        className="w-10"
        onClick={(e) => {
          // Click anywhere in the cell toggles selection — gives a
          // generous hit target without making the whole row a no-op
          // for the drawer.
          e.stopPropagation();
          onToggleSelect();
        }}
      >
        <Checkbox
          checked={selected}
          onCheckedChange={onToggleSelect}
          aria-label={t('models.selectAll')}
        />
      </TableCell>
      <TableCell className="font-mono text-xs max-w-[260px] truncate" title={model.model_id}>
        {model.model_id}
      </TableCell>
      <TableCell className="text-sm">{model.display_name}</TableCell>
      <TableCell className="text-center">
        {status === 'active' ? (
          <Badge variant="default">{t('models.status.active')}</Badge>
        ) : status === 'disabled' ? (
          <Badge
            variant="outline"
            className="border-amber-500/60 text-amber-600 dark:text-amber-400"
          >
            {t('models.status.disabled')}
          </Badge>
        ) : (
          <Badge variant="outline" className="text-muted-foreground">
            {t('models.status.unrouted')}
          </Badge>
        )}
      </TableCell>
      <TableCell className="text-right font-mono text-xs tabular-nums">
        {model.enabled_route_count}
        {model.route_count > model.enabled_route_count && (
          <span className="text-muted-foreground">/{model.route_count}</span>
        )}
      </TableCell>
      <TableCell className="max-w-[260px]">
        {model.providers.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {model.providers.slice(0, 3).map((name, i) => (
              <Badge key={i} variant="secondary" className="text-[10px] font-normal">
                {name}
              </Badge>
            ))}
            {model.providers.length > 3 && (
              <span className="text-[10px] text-muted-foreground">
                +{model.providers.length - 3}
              </span>
            )}
          </div>
        ) : (
          <span className="text-xs italic text-muted-foreground">—</span>
        )}
      </TableCell>
      <TableCell className="text-right whitespace-nowrap">
        <Button
          variant="ghost"
          size="icon"
          onClick={onOpen}
          aria-label={t('common.edit')}
          disabled={!hasPermission('models:write')}
        >
          <Pencil className="h-4 w-4" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={onDelete}
          aria-label={t('common.delete')}
          disabled={!hasPermission('models:write')}
        >
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      </TableCell>
    </TableRow>
  );
}

/* ---------- cost preview ---------- */

/// Inline helper under the weight input showing `baseline × weight = $X/M tokens`.
/// Baseline comes from the platform_pricing singleton; when it's unavailable
/// (e.g. no settings:read permission) the preview just renders nothing.
function CostPreview({
  weight,
  basePerToken,
  currency,
  side,
}: {
  weight: string;
  basePerToken: string | undefined;
  currency: string | undefined;
  side: 'input' | 'output';
}) {
  const { t } = useTranslation();
  if (!basePerToken) return null;
  const w = Number(weight);
  const base = Number(basePerToken);
  if (!Number.isFinite(w) || !Number.isFinite(base) || w <= 0 || base < 0) return null;
  const perMillion = w * base * 1_000_000;
  return (
    <p className="text-[11px] text-muted-foreground font-mono">
      {t(`models.costPreview.${side}` as 'models.costPreview.input', {
        amount: perMillion.toFixed(4),
        currency: currency ?? 'USD',
      })}
    </p>
  );
}

