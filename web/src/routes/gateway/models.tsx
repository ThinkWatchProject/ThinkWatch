import { Fragment, useCallback, useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
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
  AlertCircle,
  Brain,
  ChevronDown,
  ChevronRight,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
} from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { api, apiDelete, apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';

/* ---------- types ---------- */

interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
  input_price: string | null;
  output_price: string | null;
  input_multiplier: string;
  output_multiplier: string;
  is_active: boolean;
}

interface ModelRoute {
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

interface ModelFormState {
  display_name: string;
  input_price: string;
  output_price: string;
  input_multiplier: string;
  output_multiplier: string;
  is_active: boolean;
}

const emptyModelForm: ModelFormState = {
  display_name: '',
  input_price: '',
  output_price: '',
  input_multiplier: '1.0',
  output_multiplier: '1.0',
  is_active: true,
};

interface RouteFormState {
  provider_id: string;
  upstream_model: string;
  weight: string;
  priority: string;
}

const emptyRouteForm: RouteFormState = {
  provider_id: '',
  upstream_model: '',
  weight: '100',
  priority: '0',
};

interface RouteEditFormState {
  upstream_model: string;
  weight: string;
  priority: string;
  enabled: boolean;
}

/* ---------- component ---------- */

export function ModelsPage() {
  const { t } = useTranslation();
  const [models, setModels] = useState<ModelRow[]>([]);
  const [totalModels, setTotalModels] = useState(0);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);

  // Routes: cached per model_id
  const [expandedModel, setExpandedModel] = useState<string | null>(null);
  const [modelRoutes, setModelRoutes] = useState<Record<string, ModelRoute[]>>({});
  const [routesLoading, setRoutesLoading] = useState(false);

  // Model edit dialog
  const [modelDialogOpen, setModelDialogOpen] = useState(false);
  const [editingModel, setEditingModel] = useState<ModelRow | null>(null);
  const [modelForm, setModelForm] = useState<ModelFormState>(emptyModelForm);
  const [modelFormError, setModelFormError] = useState('');
  const [modelSaving, setModelSaving] = useState(false);

  // Model delete
  const [deleteModelId, setDeleteModelId] = useState<string | null>(null);

  // Add route dialog
  const [addRouteDialogOpen, setAddRouteDialogOpen] = useState(false);
  const [addRouteModelId, setAddRouteModelId] = useState<string | null>(null);
  const [routeForm, setRouteForm] = useState<RouteFormState>(emptyRouteForm);
  const [routeFormError, setRouteFormError] = useState('');
  const [routeSaving, setRouteSaving] = useState(false);

  // Edit route dialog
  const [editRouteDialogOpen, setEditRouteDialogOpen] = useState(false);
  const [editingRoute, setEditingRoute] = useState<ModelRoute | null>(null);
  const [routeEditForm, setRouteEditForm] = useState<RouteEditFormState>({
    upstream_model: '',
    weight: '0',
    priority: '0',
    enabled: true,
  });
  const [routeEditFormError, setRouteEditFormError] = useState('');
  const [routeEditSaving, setRouteEditSaving] = useState(false);

  // Delete route
  const [deleteRouteId, setDeleteRouteId] = useState<string | null>(null);
  const [deleteRouteModelId, setDeleteRouteModelId] = useState<string | null>(null);

  // Sync
  const [syncingAll, setSyncingAll] = useState(false);

  /* ---------- data fetching ---------- */

  const fetchModels = useCallback(async (p = page, q = search, ps = pageSize) => {
    setLoading(true);
    try {
      const params = new URLSearchParams({ page: String(p), page_size: String(ps) });
      if (q) params.set('q', q);
      const [res, provs] = await Promise.all([
        api<{ items: ModelRow[]; total: number }>(`/api/admin/models?${params}`),
        api<Provider[]>('/api/admin/providers'),
      ]);
      setModels(res.items);
      setTotalModels(res.total);
      setProviders(provs);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load models');
    } finally {
      setLoading(false);
    }
  }, [page, search, pageSize]);

  useEffect(() => {
    void fetchModels();
  }, [fetchModels]);

  const fetchRoutes = useCallback(async (modelId: string) => {
    setRoutesLoading(true);
    try {
      const routes = await api<ModelRoute[]>(`/api/admin/models/${encodeURIComponent(modelId)}/routes`);
      setModelRoutes((prev) => ({ ...prev, [modelId]: routes }));
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to load routes');
    } finally {
      setRoutesLoading(false);
    }
  }, []);

  const toggleExpand = (modelId: string) => {
    if (expandedModel === modelId) {
      setExpandedModel(null);
    } else {
      setExpandedModel(modelId);
      if (!modelRoutes[modelId]) {
        void fetchRoutes(modelId);
      }
    }
  };

  /* ---------- sync ---------- */

  const syncAll = async () => {
    setSyncingAll(true);
    let totalCount = 0;
    try {
      for (const p of providers) {
        const res = await apiPost<{ synced: number }>(`/api/admin/providers/${p.id}/sync-models`, {});
        totalCount += res.synced;
      }
      toast.success(t('models.syncSuccess', { count: totalCount }));
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Sync failed');
    } finally {
      setSyncingAll(false);
    }
  };

  /* ---------- model edit ---------- */

  const openEditModel = (m: ModelRow) => {
    setEditingModel(m);
    setModelForm({
      display_name: m.display_name,
      input_price: m.input_price ?? '',
      output_price: m.output_price ?? '',
      input_multiplier: m.input_multiplier,
      output_multiplier: m.output_multiplier,
      is_active: m.is_active,
    });
    setModelFormError('');
    setModelDialogOpen(true);
  };

  const submitModel = async (e: FormEvent) => {
    e.preventDefault();
    setModelFormError('');
    const inMult = Number(modelForm.input_multiplier);
    const outMult = Number(modelForm.output_multiplier);
    if (!Number.isFinite(inMult) || inMult <= 0 || !Number.isFinite(outMult) || outMult <= 0) {
      setModelFormError(t('models.errors.multiplierMustBePositive'));
      return;
    }
    const body = {
      display_name: modelForm.display_name,
      input_price: modelForm.input_price === '' ? null : modelForm.input_price,
      output_price: modelForm.output_price === '' ? null : modelForm.output_price,
      input_multiplier: modelForm.input_multiplier,
      output_multiplier: modelForm.output_multiplier,
      is_active: modelForm.is_active,
    };
    setModelSaving(true);
    try {
      if (editingModel) {
        await apiPatch(`/api/admin/models/${editingModel.id}`, body);
        toast.success(t('models.toast.updated'));
      }
      setModelDialogOpen(false);
      await fetchModels();
    } catch (err) {
      setModelFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setModelSaving(false);
    }
  };

  const confirmDeleteModel = async () => {
    if (!deleteModelId) return;
    try {
      await apiDelete(`/api/admin/models/${deleteModelId}`);
      toast.success(t('models.toast.deleted'));
      setDeleteModelId(null);
      if (expandedModel === deleteModelId) setExpandedModel(null);
      await fetchModels();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
    }
  };

  /* ---------- add route ---------- */

  const openAddRoute = (modelId: string) => {
    setAddRouteModelId(modelId);
    setRouteForm(emptyRouteForm);
    setRouteFormError('');
    setAddRouteDialogOpen(true);
  };

  const submitAddRoute = async (e: FormEvent) => {
    e.preventDefault();
    if (!addRouteModelId) return;
    setRouteFormError('');
    const weight = Number(routeForm.weight);
    const priority = Number(routeForm.priority);
    if (!routeForm.provider_id) {
      setRouteFormError(t('models.field.provider') + ' is required');
      return;
    }
    if (!Number.isFinite(weight) || weight < 0) {
      setRouteFormError(t('models.weight') + ' must be >= 0');
      return;
    }
    const body: Record<string, unknown> = {
      provider_id: routeForm.provider_id,
      weight,
      priority,
    };
    if (routeForm.upstream_model.trim()) {
      body.upstream_model = routeForm.upstream_model.trim();
    }
    setRouteSaving(true);
    try {
      await apiPost(`/api/admin/models/${encodeURIComponent(addRouteModelId)}/routes`, body);
      toast.success(t('models.routeAdded'));
      setAddRouteDialogOpen(false);
      await fetchRoutes(addRouteModelId);
    } catch (err) {
      setRouteFormError(err instanceof Error ? err.message : 'Failed to add route');
    } finally {
      setRouteSaving(false);
    }
  };

  /* ---------- edit route ---------- */

  const openEditRoute = (route: ModelRoute) => {
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
      await fetchRoutes(editingRoute.model_id);
    } catch (err) {
      setRouteEditFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setRouteEditSaving(false);
    }
  };

  /* ---------- delete route ---------- */

  const confirmDeleteRoute = async () => {
    if (!deleteRouteId || !deleteRouteModelId) return;
    try {
      await apiDelete(`/api/admin/model-routes/${deleteRouteId}`);
      toast.success(t('models.routeDeleted'));
      setDeleteRouteId(null);
      await fetchRoutes(deleteRouteModelId);
      setDeleteRouteModelId(null);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete route');
    }
  };

  /* ---------- helpers ---------- */

  const totalWeight = (routes: ModelRoute[]) =>
    routes.reduce((sum, r) => sum + r.weight, 0);

  const weightPct = (weight: number, total: number) =>
    total > 0 ? `${Math.round((weight / total) * 100)}%` : '0%';

  /* ---------- render ---------- */

  return (
    <div className="flex flex-col h-[calc(100vh-4rem)]">
      {/* Header */}
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('models.title')}</h1>
          <p className="text-muted-foreground">{t('models.subtitle')}</p>
        </div>
        <Button
          variant="outline"
          disabled={syncingAll || providers.length === 0}
          onClick={syncAll}
        >
          {syncingAll ? (
            <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" />
          ) : (
            <RefreshCw className="mr-1 h-3.5 w-3.5" />
          )}
          {syncingAll ? t('models.syncing') : t('models.syncAll')}
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

      {/* Search */}
      <div className="flex items-center gap-2 mb-4">
        <Input
          placeholder={t('models.searchPlaceholder')}
          value={search}
          onChange={(e) => { setSearch(e.target.value); setPage(1); }}
          className="max-w-sm"
        />
      </div>

      {/* Models table */}
      {loading ? (
        <div className="space-y-4">
          {[...Array(3)].map((_, i) => (
            <Skeleton key={i} className="h-12 w-full" />
          ))}
        </div>
      ) : models.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Brain className="mb-3 h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('models.noModels')}</p>
            <p className="mt-1 text-xs text-muted-foreground">{t('models.noModelsHint')}</p>
          </CardContent>
        </Card>
      ) : (
        <Card className="flex flex-col min-h-0 flex-1">
          <CardContent className="p-0 overflow-auto flex-1">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-8" />
                  <TableHead>{t('models.col.modelId')}</TableHead>
                  <TableHead>{t('models.col.displayName')}</TableHead>
                  <TableHead className="text-right">{t('models.col.inputPrice')}</TableHead>
                  <TableHead className="text-right">{t('models.col.outputPrice')}</TableHead>
                  <TableHead className="text-right">{t('models.col.inputMult')}</TableHead>
                  <TableHead className="text-right">{t('models.col.outputMult')}</TableHead>
                  <TableHead className="text-center">{t('models.col.active')}</TableHead>
                  <TableHead className="text-right">{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {models.map((m) => {
                  const isExpanded = expandedModel === m.model_id;
                  const routes = modelRoutes[m.model_id];
                  const tw = routes ? totalWeight(routes) : 0;
                  return (
                    <Fragment key={m.id}>
                      {/* Model row */}
                      <TableRow
                        className="cursor-pointer"
                        onClick={() => toggleExpand(m.model_id)}
                      >
                        <TableCell className="w-8 pr-0">
                          {isExpanded ? (
                            <ChevronDown className="h-4 w-4" />
                          ) : (
                            <ChevronRight className="h-4 w-4" />
                          )}
                        </TableCell>
                        <TableCell className="font-mono text-xs max-w-[220px] truncate" title={m.model_id}>{m.model_id}</TableCell>
                        <TableCell className="max-w-[200px] truncate" title={m.display_name}>{m.display_name}</TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.input_price ?? '\u2014'}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.output_price ?? '\u2014'}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.input_multiplier}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.output_multiplier}
                        </TableCell>
                        <TableCell className="text-center">
                          {m.is_active ? (
                            <Badge variant="default">{t('common.yes')}</Badge>
                          ) : (
                            <Badge variant="outline">{t('common.no')}</Badge>
                          )}
                        </TableCell>
                        <TableCell className="text-right">
                          <Button
                            variant="ghost"
                            size="icon"
                            onClick={(e) => {
                              e.stopPropagation();
                              openEditModel(m);
                            }}
                          >
                            <Pencil className="h-4 w-4" />
                          </Button>
                          <Button
                            variant="ghost"
                            size="icon"
                            onClick={(e) => {
                              e.stopPropagation();
                              setDeleteModelId(m.id);
                            }}
                          >
                            <Trash2 className="h-4 w-4 text-destructive" />
                          </Button>
                        </TableCell>
                      </TableRow>

                      {/* Expanded routes */}
                      {isExpanded && (
                        <TableRow>
                          <TableCell colSpan={9} className="bg-muted/30 p-4">
                            <div className="space-y-3">
                              <div className="flex items-center justify-between">
                                <span className="text-sm font-medium">{t('models.routes')}</span>
                                <Button
                                  variant="outline"
                                  size="sm"
                                  onClick={() => openAddRoute(m.model_id)}
                                >
                                  <Plus className="mr-1 h-3.5 w-3.5" />
                                  {t('models.addRoute')}
                                </Button>
                              </div>

                              {routesLoading && !routes ? (
                                <Skeleton className="h-8 w-full" />
                              ) : !routes || routes.length === 0 ? (
                                <p className="text-sm text-muted-foreground">
                                  {t('models.noRoutes')}
                                </p>
                              ) : (
                                <Table>
                                  <TableHeader>
                                    <TableRow>
                                      <TableHead>{t('models.field.provider')}</TableHead>
                                      <TableHead className="text-right">
                                        {t('models.weight')}
                                      </TableHead>
                                      <TableHead>{t('models.priority')}</TableHead>
                                      <TableHead>{t('models.upstreamModel')}</TableHead>
                                      <TableHead className="text-center">
                                        {t('models.col.active')}
                                      </TableHead>
                                      <TableHead className="text-right">
                                        {t('common.actions')}
                                      </TableHead>
                                    </TableRow>
                                  </TableHeader>
                                  <TableBody>
                                    {routes.map((r) => (
                                      <TableRow key={r.id}>
                                        <TableCell className="text-sm">
                                          {r.provider_name}
                                        </TableCell>
                                        <TableCell className="text-right font-mono text-xs">
                                          {weightPct(r.weight, tw)}
                                        </TableCell>
                                        <TableCell>
                                          <Badge
                                            variant={
                                              r.priority === 0 ? 'default' : 'secondary'
                                            }
                                          >
                                            {r.priority === 0
                                              ? t('models.primary')
                                              : t('models.fallback')}
                                          </Badge>
                                        </TableCell>
                                        <TableCell className="font-mono text-xs">
                                          {r.upstream_model || '\u2014'}
                                        </TableCell>
                                        <TableCell className="text-center">
                                          {r.enabled ? (
                                            <Badge variant="default">{t('common.yes')}</Badge>
                                          ) : (
                                            <Badge variant="outline">{t('common.no')}</Badge>
                                          )}
                                        </TableCell>
                                        <TableCell className="text-right">
                                          <Button
                                            variant="ghost"
                                            size="icon"
                                            onClick={() => openEditRoute(r)}
                                          >
                                            <Pencil className="h-4 w-4" />
                                          </Button>
                                          <Button
                                            variant="ghost"
                                            size="icon"
                                            onClick={() => {
                                              setDeleteRouteId(r.id);
                                              setDeleteRouteModelId(r.model_id);
                                            }}
                                          >
                                            <Trash2 className="h-4 w-4 text-destructive" />
                                          </Button>
                                        </TableCell>
                                      </TableRow>
                                    ))}
                                  </TableBody>
                                </Table>
                              )}
                            </div>
                          </TableCell>
                        </TableRow>
                      )}
                    </Fragment>
                  );
                })}
              </TableBody>
            </Table>
          </CardContent>
          <div className="border-t">
            <DataTablePagination
              total={totalModels}
              page={page}
              pageSize={pageSize}
              onPageChange={setPage}
              onPageSizeChange={setPageSize}
            />
          </div>
        </Card>
      )}

      {/* Edit Model Dialog */}
      <Dialog open={modelDialogOpen} onOpenChange={setModelDialogOpen}>
        <DialogContent className="sm:max-w-lg">
          <form onSubmit={submitModel}>
            <DialogHeader>
              <DialogTitle>{t('models.editTitle')}</DialogTitle>
              <DialogDescription>{t('models.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="display_name">{t('models.field.displayName')}</Label>
                <Input
                  id="display_name"
                  value={modelForm.display_name}
                  onChange={(e) => setModelForm({ ...modelForm, display_name: e.target.value })}
                  placeholder="GPT-4o"
                  required
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="input_price">{t('models.field.inputPrice')}</Label>
                  <Input
                    id="input_price"
                    value={modelForm.input_price}
                    onChange={(e) => setModelForm({ ...modelForm, input_price: e.target.value })}
                    placeholder="0.0025"
                    inputMode="decimal"
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="output_price">{t('models.field.outputPrice')}</Label>
                  <Input
                    id="output_price"
                    value={modelForm.output_price}
                    onChange={(e) => setModelForm({ ...modelForm, output_price: e.target.value })}
                    placeholder="0.01"
                    inputMode="decimal"
                  />
                </div>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="input_multiplier">{t('models.field.inputMult')}</Label>
                  <Input
                    id="input_multiplier"
                    value={modelForm.input_multiplier}
                    onChange={(e) =>
                      setModelForm({ ...modelForm, input_multiplier: e.target.value })
                    }
                    inputMode="decimal"
                    required
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="output_multiplier">{t('models.field.outputMult')}</Label>
                  <Input
                    id="output_multiplier"
                    value={modelForm.output_multiplier}
                    onChange={(e) =>
                      setModelForm({ ...modelForm, output_multiplier: e.target.value })
                    }
                    inputMode="decimal"
                    required
                  />
                </div>
              </div>
              <p className="text-xs text-muted-foreground">{t('models.multiplierHint')}</p>
              <div className="flex items-center gap-2">
                <Switch
                  id="is_active"
                  checked={modelForm.is_active}
                  onCheckedChange={(v) => setModelForm({ ...modelForm, is_active: v })}
                />
                <Label htmlFor="is_active">{t('models.field.active')}</Label>
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

      {/* Add Route Dialog */}
      <Dialog open={addRouteDialogOpen} onOpenChange={setAddRouteDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitAddRoute}>
            <DialogHeader>
              <DialogTitle>{t('models.addRoute')}</DialogTitle>
              <DialogDescription>{t('models.upstreamModelHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label>{t('models.field.provider')}</Label>
                <Select
                  value={routeForm.provider_id}
                  onValueChange={(v) => setRouteForm({ ...routeForm, provider_id: v })}
                >
                  <SelectTrigger>
                    <SelectValue placeholder={t('models.field.provider')} />
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
              <div className="space-y-2">
                <Label htmlFor="route_upstream">{t('models.upstreamModel')}</Label>
                <Input
                  id="route_upstream"
                  value={routeForm.upstream_model}
                  onChange={(e) => setRouteForm({ ...routeForm, upstream_model: e.target.value })}
                  placeholder={t('models.upstreamModelHint')}
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="route_weight">{t('models.weight')}</Label>
                  <Input
                    id="route_weight"
                    value={routeForm.weight}
                    onChange={(e) => setRouteForm({ ...routeForm, weight: e.target.value })}
                    inputMode="numeric"
                    required
                  />
                </div>
                <div className="space-y-2">
                  <Label>{t('models.priority')}</Label>
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
              {routeFormError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{routeFormError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => setAddRouteDialogOpen(false)}
              >
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={routeSaving}>
                {routeSaving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Edit Route Dialog */}
      <Dialog open={editRouteDialogOpen} onOpenChange={setEditRouteDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submitEditRoute}>
            <DialogHeader>
              <DialogTitle>{t('models.editTitle')}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="edit_upstream">{t('models.upstreamModel')}</Label>
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
                  <Label htmlFor="edit_weight">{t('models.weight')}</Label>
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
                  <Label>{t('models.priority')}</Label>
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

      {/* Delete Model Confirm */}
      <ConfirmDialog
        open={deleteModelId !== null}
        onOpenChange={(o) => !o && setDeleteModelId(null)}
        title={t('models.deleteTitle')}
        description={t('models.deleteConfirm')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDeleteModel}
      />

      {/* Delete Route Confirm */}
      <ConfirmDialog
        open={deleteRouteId !== null}
        onOpenChange={(o) => {
          if (!o) {
            setDeleteRouteId(null);
            setDeleteRouteModelId(null);
          }
        }}
        title={t('models.deleteTitle')}
        description={t('models.routeDeleted')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDeleteRoute}
      />
    </div>
  );
}
