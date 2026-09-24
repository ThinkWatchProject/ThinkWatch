import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Skeleton } from '@/components/ui/skeleton';
import { Pencil, Plus, Trash2 } from 'lucide-react';
import { hasPermission } from '@/lib/api';
import { LatencySparkline } from '../routing/LatencySparkline';
import { RoutingModeSection } from '../routing/RoutingModeSection';
import { TrafficBar } from '../routing/TrafficBar';
import { CostPreview } from './CostPreview';
import {
  CACHE_WEIGHTS,
  derivedCacheWeight,
  modelStatus,
  type CacheWeight,
  type ModelRow,
  type PlatformPricing,
  type RouteHealthEntry,
  type RouteRow,
  type RoutingStrategy,
} from './types';

const CACHE_COL_LABEL: Record<CacheWeight, string> = {
  cache_read_weight: 'models.col.cacheRead',
  cache_write_weight: 'models.col.cacheWrite',
  cache_write_1h_weight: 'models.col.cacheWrite1h',
};

/// Right-side drawer with one model's basics (weights, cost preview,
/// edit/delete actions) and routes list (per-route health,
/// latency, bulk-enable, weight rebalance). Open state is parent-
/// controlled; we just receive `model` (null when closed) and the
/// callbacks for every interaction.
export function ModelDetailSheet({
  model,
  routes,
  routesLoading,
  routeHealth,
  globalStrategy,
  pricing,
  providerLabel,
  onClose,
  onEditModel,
  onDeleteModel,
  onAddRoute,
  onEditRoute,
  onDeleteRoute,
  onSetAllRoutesEnabled,
  onUpdateModelStrategy,
  onBatchUpdateWeights,
}: {
  /// Currently-open model. `null` ⇒ sheet is closed.
  model: ModelRow | null;
  /// Routes for the open model — `undefined` while the parent's
  /// lazy fetch hasn't landed yet.
  routes: RouteRow[] | undefined;
  routesLoading: boolean;
  /// Live-polled per-route health, keyed by route_id. Stays at
  /// the parent so it can be shared with the route editor dialog;
  /// re-renders happen as the polling state changes.
  routeHealth: Record<string, RouteHealthEntry>;
  /// Global default routing strategy — used by RoutingModeSection
  /// to show admins whether the model is diverging from fleet
  /// policy.
  globalStrategy: RoutingStrategy;
  pricing: PlatformPricing | null;
  /// Maps provider_id → display name. Lifted up since the route
  /// table needs it and the parent already has the providers list.
  providerLabel: (id: string) => string;
  onClose: () => void;
  onEditModel: (m: ModelRow) => void;
  onDeleteModel: (m: ModelRow) => void;
  onAddRoute: (m: ModelRow) => void;
  onEditRoute: (r: RouteRow) => void;
  onDeleteRoute: (r: RouteRow) => void;
  onSetAllRoutesEnabled: (modelId: string, enabled: boolean) => void;
  onUpdateModelStrategy: (modelId: string, strategy: RoutingStrategy | null) => void;
  onBatchUpdateWeights: (
    modelId: string,
    updates: Array<{ id: string; weight: number }>,
  ) => void;
}) {
  const { t } = useTranslation();
  return (
    <Sheet
      open={model !== null}
      onOpenChange={(o) => {
        if (!o) onClose();
      }}
    >
      <SheetContent className="w-full sm:max-w-xl overflow-y-auto">
        {model &&
          (() => {
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
                          onClick={() => onEditModel(model)}
                        >
                          <Pencil className="mr-1 h-3 w-3" />
                          {t('common.edit')}
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 text-xs text-destructive hover:text-destructive"
                          onClick={() => onDeleteModel(model)}
                        >
                          <Trash2 className="mr-1 h-3 w-3" />
                          {t('common.delete')}
                        </Button>
                      </div>
                    </div>
                    <div className="grid grid-cols-2 gap-2 text-xs">
                      <div>
                        <div className="text-muted-foreground">{t('models.col.inputWeight')}</div>
                        <div className="font-mono tabular-nums">{model.input_weight}</div>
                      </div>
                      <div>
                        <div className="text-muted-foreground">{t('models.col.outputWeight')}</div>
                        <div className="font-mono tabular-nums">{model.output_weight}</div>
                      </div>
                    </div>
                    <div className="grid grid-cols-3 gap-2 text-xs">
                      {CACHE_WEIGHTS.map((k) => (
                        <div key={k}>
                          <div className="text-muted-foreground">{t(CACHE_COL_LABEL[k])}</div>
                          <div className="font-mono tabular-nums">
                            {model[k] ??
                              t('models.derivedWeight', {
                                value: derivedCacheWeight(model.input_weight, k),
                              })}
                          </div>
                        </div>
                      ))}
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
                        modelStrategy={(model.routing_strategy as RoutingStrategy | null) ?? null}
                        globalStrategy={globalStrategy}
                        disabled={!hasPermission('models:write')}
                        onChange={(next) => onUpdateModelStrategy(model.model_id, next)}
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
                                onBatchUpdateWeights(model.model_id, updates)
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
                            onClick={() => onSetAllRoutesEnabled(model.model_id, true)}
                          >
                            {t('models.enableAllRoutes')}
                          </Button>
                        )}
                        {routes && routes.some((r) => r.enabled) && (
                          <Button
                            variant="outline"
                            size="sm"
                            className="h-7 text-xs"
                            onClick={() => onSetAllRoutesEnabled(model.model_id, false)}
                          >
                            {t('models.disableAllRoutes')}
                          </Button>
                        )}
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 text-xs"
                          onClick={() => onAddRoute(model)}
                        >
                          <Plus className="mr-1 h-3 w-3" />
                          {t('models.addRoute')}
                        </Button>
                      </div>
                    </div>
                    {routesLoading ? (
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
                            {routes.map((r) => (
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
                                    <LatencySparkline modelId={r.model_id} routeId={r.id} />
                                    {(() => {
                                      const ewma = routeHealth[r.id]?.health?.ewma_latency_ms;
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
                                    onClick={() => onEditRoute(r)}
                                    aria-label={t('common.edit')}
                                    title={t('common.edit')}
                                  >
                                    <Pencil className="h-3.5 w-3.5" />
                                  </Button>
                                  <Button
                                    variant="ghost"
                                    size="icon"
                                    className="h-7 w-7"
                                    onClick={() => onDeleteRoute(r)}
                                    aria-label={t('common.delete')}
                                    title={t('common.delete')}
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
                </div>
              </>
            );
          })()}
      </SheetContent>
    </Sheet>
  );
}
