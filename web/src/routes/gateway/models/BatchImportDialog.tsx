import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useResetOnChange } from '@/hooks/use-reset-on-change';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { AlertCircle, Loader2, Search } from 'lucide-react';
import { api, apiPost } from '@/lib/api';
import { toast } from 'sonner';
import type { Provider } from '../provider-types';
import type { RouteRow } from './types';

/// Per-upstream decision in step 2. `target_model_id` non-null ⇒
/// attach the new route to that existing exposed model; null ⇒ make
/// a brand-new catalog entry, with the (optional) `new_model_id`
/// aliasing the exposed id (e.g. expose `deepseek/deepseek-v4-flash`
/// as just `deepseek-v4`). Empty falls back to the upstream name.
type ImportDecision = { target_model_id: string | null; new_model_id?: string };

/// Two-step batch import: pick provider + tick remote models, then
/// decide per-item "new catalog entry vs attach to existing exposed
/// model". Owns ALL of its working state internally — parent only
/// needs to set `open` + provide `initialProviderId` for the deeplink
/// case + handle `onSaved` (refetch models list).
///
/// `onSavedForModel` fires once per import; the parent uses it to
/// refresh the detail-drawer routes if a touched model is open.
export function BatchImportDialog({
  open,
  initialProviderId,
  providers,
  detailModelId,
  onClose,
  onSaved,
  onSavedForModel,
}: {
  open: boolean;
  initialProviderId?: string | null;
  providers: Provider[];
  /// model_id of the currently-open detail drawer (or null). Used to
  /// know which (if any) routes list to refresh after the batch
  /// import lands.
  detailModelId: string | null;
  onClose: () => void;
  onSaved: () => Promise<void> | void;
  onSavedForModel: (modelId: string) => Promise<void> | void;
}) {
  const { t } = useTranslation();
  const [step, setStep] = useState<1 | 2>(1);
  const [providerId, setProviderId] = useState('');
  const [remoteModels, setRemoteModels] = useState<string[]>([]);
  const [unavailable, setUnavailable] = useState<Map<string, string>>(new Map());
  const [remoteModelsLoading, setRemoteModelsLoading] = useState(false);
  const [remoteModelsError, setRemoteModelsError] = useState('');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState('');
  const [saving, setSaving] = useState(false);
  const [existingIds, setExistingIds] = useState<Set<string>>(new Set());
  // Catalog list for step 2's "attach to existing" picker — fetched
  // once per open so stepping back and forward is instant.
  const [catalogModels, setCatalogModels] = useState<
    { model_id: string; display_name: string }[]
  >([]);
  const [decisions, setDecisions] = useState<Record<string, ImportDecision>>({});

  // Reset on open. Catalog list is fetched here too so step 2 has
  // it ready by the time the user gets there.
  // The reset happens during render; the catalog fetch stays in an effect
  // below, since that is a genuine side effect rather than state alignment.
  useResetOnChange(open, () => {
    if (!open) return;
    setStep(1);
    setProviderId('');
    setRemoteModels([]);
    setRemoteModelsError('');
    setSelected(new Set());
    setSearch('');
    setExistingIds(new Set());
    setDecisions({});
  });

  useEffect(() => {
    if (!open) return;
    void api<{ model_id: string; display_name: string }[]>('/api/admin/models/ids')
      .then(setCatalogModels)
      .catch(() => setCatalogModels([]));
  }, [open]);

  const onProviderChange = useCallback(async (pid: string) => {
    setProviderId(pid);
    setSelected(new Set());
    setSearch('');
    setRemoteModels([]);
    setUnavailable(new Map());
    setRemoteModelsError('');
    setExistingIds(new Set());

    if (!pid) return;

    setRemoteModelsLoading(true);
    try {
      const [rmodels, existing] = await Promise.all([
        api<{ id: string; available?: boolean; reason?: string }[]>(
          `/api/admin/providers/${pid}/remote-models`,
        ),
        api<{ items: RouteRow[]; total: number }>(
          `/api/admin/model-routes?provider_id=${pid}&page=1&page_size=10000`,
        ),
      ]);
      setRemoteModels(rmodels.map((m) => m.id));
      // Models this upstream has already told us it won't serve. They
      // are shown but not selectable — see `importable`.
      setUnavailable(
        new Map(
          rmodels
            .filter((m) => m.available === false)
            .map((m) => [m.id, m.reason ?? '']),
        ),
      );
      // A remote name counts as "already imported" when it appears
      // as either a route's exposed model_id (new-catalog-entry
      // imports) or its upstream_model (attach-to-existing imports
      // — where model_id is the rename target, so a model_id-only
      // check would miss it).
      const seen = new Set<string>();
      for (const r of existing.items) {
        seen.add(r.model_id);
        seen.add(r.upstream_model);
      }
      setExistingIds(seen);
    } catch (err) {
      setRemoteModelsError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setRemoteModelsLoading(false);
    }
  }, [t]);

  // Deeplink: when the dialog opens with an `initialProviderId`
  // (`?import=<providerId>` query param landed by the Providers
  // page), auto-select that provider and kick its remote-models
  // fetch.
  useEffect(() => {
    if (!open || !initialProviderId) return;
    if (!providers.some((p) => p.id === initialProviderId)) return;
    // Hand-rolled load: the spinner flag is the first half of "start a
    // fetch" and belongs with it. See "Data fetching" in web/README.md —
    // this goes away with a data-fetching layer, not by moving the flag.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void onProviderChange(initialProviderId);
  }, [open, initialProviderId, providers, onProviderChange]);

  /// Heuristic for "did the admin probably mean to attach this to
  /// an already-exposed model, or to make a new one?". Matches on
  /// exact name, else substring, else defaults to "new".
  const suggestDecision = (
    upstream: string,
    catalog: { model_id: string }[],
  ): ImportDecision => {
    const exact = catalog.find((c) => c.model_id === upstream);
    if (exact) return { target_model_id: exact.model_id };
    const partial = catalog.find(
      (c) => upstream.includes(c.model_id) || c.model_id.includes(upstream),
    );
    if (partial) return { target_model_id: partial.model_id };
    return { target_model_id: null };
  };

  const goToStep2 = () => {
    const next: Record<string, ImportDecision> = {};
    for (const u of selected) next[u] = suggestDecision(u, catalogModels);
    setDecisions(next);
    setStep(2);
  };

  const filteredRemoteModels = useMemo(() => {
    if (!search) return remoteModels;
    const q = search.toLowerCase();
    return remoteModels.filter((m) => m.toLowerCase().includes(q));
  }, [remoteModels, search]);

  /// A model is offerable only if it isn't already imported and the
  /// upstream hasn't told us it refuses to serve it. Importing a
  /// refused model can't work — the server drops it — so letting it be
  /// ticked would be a checkbox that does nothing and a count that
  /// lies. The way back for a stale verdict is the provider's
  /// "re-check models" action, not a doomed import.
  const importable = (modelId: string) =>
    !existingIds.has(modelId) && !unavailable.has(modelId);

  const toggleModel = (modelId: string) => {
    if (!importable(modelId)) return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(modelId)) next.delete(modelId);
      else next.add(modelId);
      return next;
    });
  };

  const toggleSelectAll = () => {
    const selectable = filteredRemoteModels.filter(importable);
    const allSelected = selectable.length > 0 && selectable.every((m) => selected.has(m));
    setSelected((prev) => {
      const next = new Set(prev);
      if (allSelected) for (const m of selectable) next.delete(m);
      else for (const m of selectable) next.add(m);
      return next;
    });
  };

  const submit = async () => {
    if (!providerId || selected.size === 0) return;
    setSaving(true);
    try {
      const items = Array.from(selected).map((upstream) => {
        const d = decisions[upstream] ?? { target_model_id: null };
        const newId = d.new_model_id?.trim();
        return {
          upstream,
          target_model_id: d.target_model_id,
          new_model_id: d.target_model_id === null && newId ? newId : undefined,
        };
      });
      const res = await apiPost<{
        created: number;
        skipped?: { upstream: string; reason: string }[];
      }>('/api/admin/model-routes/batch', { provider_id: providerId, items });
      toast.success(t('models.batchSuccess', { count: res.created }));
      // Never let a skipped model pass silently: the admin picked it and
      // it did not get imported, so say which ones and why.
      if (res.skipped?.length) {
        toast.warning(
          t('models.batchSkipped', {
            count: res.skipped.length,
            models: res.skipped.map((s) => s.upstream).join(', '),
          }),
          {
            // Only quote the upstream's wording when it can only be
            // about the one model. Showing the first of thirteen
            // reasons reads as if it explained all thirteen.
            description: res.skipped.length === 1 ? res.skipped[0].reason : undefined,
            duration: 10000,
          },
        );
      }
      onClose();
      await onSaved();
      // If the drawer is open on a model we just touched, refresh it.
      if (detailModelId) await onSavedForModel(detailModelId);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>
            {t('models.addRoutes')}{' '}
            <span className="text-xs font-normal text-muted-foreground">
              {t('models.stepOf', { current: step, total: 2 })}
            </span>
          </DialogTitle>
          <DialogDescription>
            {step === 1 ? t('models.batchImportHint') : t('models.batchStep2Hint')}
          </DialogDescription>
        </DialogHeader>

        {step === 1 && (
          <div className="space-y-4 py-2 flex flex-col min-h-0 flex-1">
            <Alert>
              <AlertCircle className="h-4 w-4" />
              <AlertDescription className="text-xs">
                {t('models.batchImportWarning')}
              </AlertDescription>
            </Alert>
            <div className="space-y-2">
              <Label>{t('models.selectProvider')}</Label>
              <Select value={providerId} onValueChange={onProviderChange}>
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
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                      className="pl-9"
                    />
                  </div>
                  <span className="text-sm text-muted-foreground whitespace-nowrap">
                    {t('models.selected', { count: selected.size })}
                  </span>
                  <Button type="button" variant="outline" size="sm" onClick={toggleSelectAll}>
                    {filteredRemoteModels.filter(importable).length > 0 &&
                    filteredRemoteModels.filter(importable).every((m) => selected.has(m))
                      ? t('models.deselectAll')
                      : t('models.selectAll')}
                  </Button>
                </div>
                <div className="border rounded-md overflow-auto flex-1 min-h-0 max-h-[40vh]">
                  {filteredRemoteModels.map((modelId) => {
                    const exists = existingIds.has(modelId);
                    const refused = unavailable.has(modelId);
                    const checked = exists || selected.has(modelId);
                    return (
                      <label
                        key={modelId}
                        className="flex items-center gap-2 px-3 py-1.5 hover:bg-muted/50 cursor-pointer text-sm border-b last:border-b-0"
                      >
                        <Checkbox
                          checked={checked && !refused}
                          disabled={!importable(modelId)}
                          onCheckedChange={() => toggleModel(modelId)}
                        />
                        <span
                          className={`font-mono text-xs truncate ${exists || refused ? 'text-muted-foreground' : ''}`}
                        >
                          {modelId}
                        </span>
                        {exists ? (
                          <span className="text-xs text-muted-foreground ml-auto whitespace-nowrap">
                            ({t('models.alreadyExists')})
                          </span>
                        ) : (
                          unavailable.has(modelId) && (
                            <span
                              className="text-xs text-destructive ml-auto whitespace-nowrap"
                              title={unavailable.get(modelId)}
                            >
                              ({t('models.upstreamRefuses')})
                            </span>
                          )
                        )}
                      </label>
                    );
                  })}
                </div>
              </>
            )}
          </div>
        )}

        {step === 2 && (
          <div className="space-y-3 py-2 flex flex-col min-h-0 flex-1">
            <div className="flex items-center gap-2">
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => {
                  const next = { ...decisions };
                  for (const u of selected) next[u] = { target_model_id: null };
                  setDecisions(next);
                }}
              >
                {t('models.batchAllNew')}
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => {
                  const next = { ...decisions };
                  for (const u of selected) next[u] = suggestDecision(u, catalogModels);
                  setDecisions(next);
                }}
              >
                {t('models.batchResetSuggestions')}
              </Button>
            </div>
            <div className="border rounded-md overflow-auto flex-1 min-h-0 max-h-[50vh] divide-y">
              {Array.from(selected)
                .sort()
                .map((upstream) => {
                  const decision = decisions[upstream] ?? { target_model_id: null };
                  const setDecision = (d: ImportDecision) =>
                    setDecisions({ ...decisions, [upstream]: d });
                  const isNew = decision.target_model_id === null;
                  return (
                    <div key={upstream} className="p-3 space-y-2">
                      <div className="font-mono text-xs break-all">{upstream}</div>
                      <div className="flex items-center gap-2 text-xs">
                        <Select
                          value={decision.target_model_id ?? '__new__'}
                          onValueChange={(v) => {
                            if (v === '__new__') {
                              setDecision({
                                target_model_id: null,
                                new_model_id: decision.new_model_id,
                              });
                            } else {
                              setDecision({ target_model_id: v });
                            }
                          }}
                        >
                          <SelectTrigger className="h-7 text-xs flex-1">
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            <SelectItem value="__new__">{t('models.batchModeNew')}</SelectItem>
                            {catalogModels.map((c) => (
                              <SelectItem key={c.model_id} value={c.model_id}>
                                {t('models.batchModeAttach', { target: c.model_id })}
                              </SelectItem>
                            ))}
                          </SelectContent>
                        </Select>
                      </div>
                      {isNew && (
                        <Input
                          className="h-7 text-xs font-mono"
                          placeholder={upstream}
                          value={decision.new_model_id ?? ''}
                          onChange={(e) =>
                            setDecision({
                              target_model_id: null,
                              new_model_id: e.target.value,
                            })
                          }
                          aria-label={t('models.batchNewModelIdLabel')}
                        />
                      )}
                    </div>
                  );
                })}
            </div>
          </div>
        )}

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          {step === 1 ? (
            <Button type="button" disabled={selected.size === 0} onClick={goToStep2}>
              {t('models.batchNextStep', { count: selected.size })}
            </Button>
          ) : (
            <>
              <Button type="button" variant="outline" onClick={() => setStep(1)}>
                {t('common.previous')}
              </Button>
              <Button type="button" disabled={saving} onClick={submit}>
                {saving ? <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" /> : null}
                {t('models.addNRoutes', { count: selected.size })}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
