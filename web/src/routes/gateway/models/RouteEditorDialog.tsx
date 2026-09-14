import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { useResetOnChange } from '@/hooks/use-reset-on-change';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
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
import { Switch } from '@/components/ui/switch';
import { AlertCircle, Loader2 } from 'lucide-react';
import { api, apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';
import {
  emptyRouteForm,
  type ModelRow,
  type RouteFormState,
  type RouteHealthEntry,
  type RouteRow,
} from './types';
import type { Provider } from '../provider-types';

/// One entry of `GET /api/admin/providers/{id}/remote-models`.
interface RemoteModel {
  id: string;
  available?: boolean;
  reason?: string;
}

const remoteModelIds = (rows: RemoteModel[]) => rows.map((r) => r.id);

/// Create/edit dialog for a Route entry on a Model. Owns its own
/// form, remote-model picker cache, and saving state. Parent only
/// needs to manage `open` + which `route`/`targetModel` is active +
/// `onSaved(modelId)` for the post-write refetch.
export function RouteEditorDialog({
  open,
  route,
  targetModel,
  providers,
  routeHealth,
  onClose,
  onSaved,
}: {
  open: boolean;
  /// Edit mode: `route` is set; `targetModel` is ignored.
  route: RouteRow | null;
  /// Create mode: `route` is null and `targetModel` is the parent
  /// model the new route attaches to.
  targetModel: ModelRow | null;
  providers: Provider[];
  /// Live-polled per-route health, keyed by route_id. The dialog
  /// only reads it for the read-only "live status" panel shown in
  /// edit mode; updates come naturally as the parent's polling
  /// state changes.
  routeHealth: Record<string, RouteHealthEntry>;
  onClose: () => void;
  onSaved: (modelId: string) => Promise<void> | void;
}) {
  const { t } = useTranslation();
  const [form, setForm] = useState<RouteFormState>(emptyRouteForm);
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);

  // Reset form on open transition.
  useResetOnChange(`${open}\u0000${route?.id ?? ''}\u0000${targetModel?.model_id ?? ''}`, () => {
    if (!open) return;
    if (route) {
      setForm({
        provider_id: route.provider_id,
        upstream_model: route.upstream_model,
        enabled: route.enabled,
        label: route.label ?? '',
        notes: route.notes ?? '',
        rpm_cap: route.rpm_cap == null ? '' : String(route.rpm_cap),
        tpm_cap: route.tpm_cap == null ? '' : String(route.tpm_cap),
      });
    } else if (targetModel) {
      setForm({ ...emptyRouteForm, upstream_model: targetModel.model_id });
    } else {
      setForm(emptyRouteForm);
    }
    setError('');
  });

  // Pull the upstream-model picker options from the selected provider's
  // remote catalog. Each lookup costs the backend a call to the
  // upstream's own model listing, so a provider's list is kept rather
  // than refetched: switching providers back and forth is instant. A
  // provider with no /models endpoint, or a temporary fetch failure,
  // falls back to free input.
  const pid = form.provider_id;
  const remoteQuery = useQuery({
    queryKey: ['admin', 'providers', pid, 'remote-models'],
    queryFn: ({ signal }) =>
      api<RemoteModel[]>(`/api/admin/providers/${pid}/remote-models`, { signal }),
    enabled: open && !!pid,
    staleTime: Infinity,
    select: remoteModelIds,
  });

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError('');
    // Empty cap → null (unlimited). Non-empty must be a positive integer.
    const parseCap = (s: string): number | null | 'invalid' => {
      const v = s.trim();
      if (!v) return null;
      const n = Number(v);
      if (!Number.isFinite(n) || n <= 0) return 'invalid';
      return Math.floor(n);
    };
    const rpm = parseCap(form.rpm_cap);
    const tpm = parseCap(form.tpm_cap);
    if (rpm === 'invalid' || tpm === 'invalid') {
      setError(t('models.errors.capMustBePositive'));
      return;
    }
    const upstream = form.upstream_model.trim();
    if (!upstream) {
      setError(t('models.col.upstreamModel') + ' is required');
      return;
    }
    setSaving(true);
    try {
      const label = form.label.trim() || null;
      const notes = form.notes.trim() || null;
      if (route) {
        await apiPatch(`/api/admin/model-routes/${route.id}`, {
          upstream_model: upstream,
          enabled: form.enabled,
          label,
          notes,
          rpm_cap: rpm,
          tpm_cap: tpm,
        });
        toast.success(t('models.toast.updated'));
        await onSaved(route.model_id);
      } else if (targetModel) {
        if (!form.provider_id) {
          setError(t('models.field.provider') + ' is required');
          setSaving(false);
          return;
        }
        // model_id may contain '/' (e.g. `deepseek/deepseek-v4-flash`),
        // so encode before injecting into the URL or axum's router
        // will see four path segments and 404.
        await apiPost(
          `/api/admin/models/${encodeURIComponent(targetModel.model_id)}/routes`,
          {
            provider_id: form.provider_id,
            upstream_model: upstream,
            enabled: form.enabled,
            label,
            notes,
            rpm_cap: rpm,
            tpm_cap: tpm,
          },
        );
        toast.success(t('models.routeAdded'));
        await onSaved(targetModel.model_id);
      }
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={handleSubmit}>
          <DialogHeader>
            <DialogTitle>
              {route ? t('models.editRouteTitle') : t('models.addRouteTitle')}
            </DialogTitle>
            <DialogDescription>
              {route ? route.model_id : (targetModel?.model_id ?? '')}
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-4 py-4">
            {!route && (
              <div className="space-y-2">
                <Label>{t('models.field.provider')}</Label>
                <Select
                  value={form.provider_id}
                  onValueChange={(v) => setForm({ ...form, provider_id: v })}
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
                const remote = remoteQuery.data;
                // Loading: provider picked, fetch in flight.
                if (remoteQuery.isLoading) {
                  return (
                    <div className="flex items-center gap-2 text-xs text-muted-foreground h-9 px-3 border rounded-md">
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      {t('models.loadingModels')}
                    </div>
                  );
                }
                // Fetched a usable list → searchable select.
                if (remote && remote.length > 0) {
                  return (
                    <Select
                      value={form.upstream_model}
                      onValueChange={(v) => setForm({ ...form, upstream_model: v })}
                    >
                      <SelectTrigger id="route_upstream">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
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
                  <Input
                    id="route_upstream"
                    value={form.upstream_model}
                    onChange={(e) => setForm({ ...form, upstream_model: e.target.value })}
                    placeholder={t('models.upstreamModelHint')}
                  />
                );
              })()}
            </div>
            <div className="space-y-2">
              <Label htmlFor="route_label">{t('models.routing.labelLabel')}</Label>
              <Input
                id="route_label"
                value={form.label}
                onChange={(e) => setForm({ ...form, label: e.target.value })}
                placeholder={t('models.routing.labelPlaceholder')}
                maxLength={64}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="route_notes">{t('models.routing.notesLabel')}</Label>
              <Input
                id="route_notes"
                value={form.notes}
                onChange={(e) => setForm({ ...form, notes: e.target.value })}
                placeholder={t('models.routing.notesPlaceholder')}
              />
            </div>
            <div className="flex items-center gap-2">
              <Switch
                id="route_enabled"
                checked={form.enabled}
                onCheckedChange={(v) => setForm({ ...form, enabled: v })}
              />
              <Label htmlFor="route_enabled">{t('models.field.active')}</Label>
            </div>
            {/* Per-route capacity caps. Empty = unlimited; both
                enforced via the same sliding-window engine that
                drives api-key/user RPM/TPM, just with a route key. */}
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-2">
                <Label htmlFor="rpm_cap">{t('models.field.rpmCap')}</Label>
                <Input
                  id="rpm_cap"
                  value={form.rpm_cap}
                  onChange={(e) => setForm({ ...form, rpm_cap: e.target.value })}
                  placeholder={t('models.unlimited')}
                  inputMode="numeric"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="tpm_cap">{t('models.field.tpmCap')}</Label>
                <Input
                  id="tpm_cap"
                  value={form.tpm_cap}
                  onChange={(e) => setForm({ ...form, tpm_cap: e.target.value })}
                  placeholder={t('models.unlimited')}
                  inputMode="numeric"
                />
              </div>
            </div>
            {/* Live status — read-only signals for the route under
                edit. Only shown when editing (we have a route_id +
                health snapshot) — the add-flow can't show this yet. */}
            {route && (
              <div className="rounded-md border bg-muted/20 p-3 space-y-1.5 text-xs">
                <div className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
                  {t('models.routing.liveStatus')}
                </div>
                <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-0.5">
                  <dt className="text-muted-foreground">{t('models.col.health')}</dt>
                  <dd>
                    {(() => {
                      const state = routeHealth[route.id]?.health?.state ?? 'closed';
                      return t(`models.health.${state}`);
                    })()}
                  </dd>
                  <dt className="text-muted-foreground">{t('models.col.p50')}</dt>
                  <dd className="font-mono">
                    {(() => {
                      const ewma = routeHealth[route.id]?.health?.ewma_latency_ms;
                      return ewma == null ? '—' : `${ewma.toFixed(0)} ms`;
                    })()}
                  </dd>
                  <dt className="text-muted-foreground">
                    {t('models.routing.errorPctLabel')}
                  </dt>
                  <dd className="font-mono">
                    {(() => {
                      const h = routeHealth[route.id]?.health;
                      if (!h || h.total === 0) return '—';
                      return `${h.error_pct.toFixed(1)}% (${h.errors}/${h.total})`;
                    })()}
                  </dd>
                </dl>
                {/* Cumulative all-time count — visually subordinate
                    to the rolling-window stats above (smaller +
                    muted) but always shown, even at 0, so operators
                    can tell apart "never used" from "quiet now". */}
                <p className="text-[10px] text-muted-foreground">
                  {t('models.routing.lifetimeLabel', {
                    count: routeHealth[route.id]?.health?.lifetime_requests ?? 0,
                    formatted: (
                      routeHealth[route.id]?.health?.lifetime_requests ?? 0
                    ).toLocaleString(),
                  })}
                </p>
              </div>
            )}
            {error && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            )}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={saving}>
              {saving ? t('common.saving') : t('common.save')}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
