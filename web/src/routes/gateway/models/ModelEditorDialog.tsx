import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
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
import { AlertCircle } from 'lucide-react';
import { apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';
import { CostPreview } from './CostPreview';
import { OutputGuardrailsCard } from './OutputGuardrailsCard';
import {
  AFFINITY_MODES,
  CACHE_WEIGHTS,
  MAX_CHARS_CEILING,
  ROUTING_STRATEGIES,
  derivedCacheWeight,
  emptyModelForm,
  parseGuardrails,
  type AffinityMode,
  type CacheWeight,
  type ModelFormState,
  type ModelRow,
  type PlatformPricing,
  type RoutingStrategy,
} from './types';

const CACHE_WEIGHT_LABEL: Record<CacheWeight, string> = {
  cache_read_weight: 'models.field.cacheReadWeight',
  cache_write_weight: 'models.field.cacheWriteWeight',
  cache_write_1h_weight: 'models.field.cacheWrite1hWeight',
};

/// Create/edit dialog for a Model catalog entry. Owns its own form
/// state + saving + error UI so the parent route only manages
/// `open`/`model` plus a single `onSaved` callback that fires after
/// the server accepts the write (parent refreshes its list).
///
/// `model == null` ⇒ create mode. Pre-populates form from `model`
/// on every open via the reset effect.
export function ModelEditorDialog({
  open,
  model,
  pricing,
  onClose,
  onSaved,
}: {
  open: boolean;
  model: ModelRow | null;
  pricing: PlatformPricing | null;
  onClose: () => void;
  onSaved: () => Promise<void> | void;
}) {
  const { t } = useTranslation();
  const [form, setForm] = useState<ModelFormState>(emptyModelForm);
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);

  // Reset form whenever the dialog opens. Without this, an admin
  // who edits model A, closes, then edits model B would see A's
  // values in B's dialog.
  useResetOnChange(`${open}\u0000${model?.model_id ?? ''}`, () => {
    if (!open) return;
    if (model) {
      setForm({
        model_id: model.model_id,
        display_name: model.display_name,
        input_weight: model.input_weight,
        output_weight: model.output_weight,
        cache_read_weight: model.cache_read_weight ?? '',
        cache_write_weight: model.cache_write_weight ?? '',
        cache_write_1h_weight: model.cache_write_1h_weight ?? '',
        routing_strategy: (model.routing_strategy ?? '') as ModelFormState['routing_strategy'],
        affinity_mode: (model.affinity_mode ?? '') as ModelFormState['affinity_mode'],
        affinity_ttl_secs: model.affinity_ttl_secs == null ? '' : String(model.affinity_ttl_secs),
        output_guardrails: parseGuardrails(model.output_guardrails),
      });
    } else {
      setForm(emptyModelForm);
    }
    setError('');
  });

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError('');
    const inW = Number(form.input_weight);
    const outW = Number(form.output_weight);
    if (!Number.isFinite(inW) || inW <= 0 || !Number.isFinite(outW) || outW <= 0) {
      setError(t('models.errors.weightMustBePositive'));
      return;
    }
    // Cache weights: empty ⇒ null ⇒ derived from the input weight.
    const cacheWeights = {} as Record<CacheWeight, number | null>;
    for (const k of CACHE_WEIGHTS) {
      const raw = form[k].trim();
      const n = raw ? Number(raw) : null;
      if (n != null && (!Number.isFinite(n) || n < 0)) {
        setError(t('models.errors.cacheWeightNotNegative'));
        return;
      }
      cacheWeights[k] = n;
    }
    // Routing overrides: empty string in the form ⇒ JSON null on the
    // wire ⇒ "inherit global default" (PATCH semantics).
    const ttl = form.affinity_ttl_secs.trim();
    const ttlNum = ttl ? Number(ttl) : null;
    if (ttlNum != null && (!Number.isFinite(ttlNum) || ttlNum < 0 || ttlNum > 86400)) {
      setError(t('models.errors.affinityTtlRange'));
      return;
    }
    // Mirror the server's `validate_output_guardrails`: every
    // max_length entry must be 1..=MAX_CHARS_CEILING. Client-side
    // check gives a snappier error than a 400 round trip.
    for (const g of form.output_guardrails) {
      if (g.type === 'max_length') {
        if (!Number.isInteger(g.max_chars) || g.max_chars < 1 || g.max_chars > MAX_CHARS_CEILING) {
          setError(t('models.outputGuardrails.maxLengthRange', { max: MAX_CHARS_CEILING }));
          return;
        }
      }
    }
    const body = {
      display_name: form.display_name.trim() || form.model_id.trim(),
      input_weight: inW,
      output_weight: outW,
      ...cacheWeights,
      routing_strategy: form.routing_strategy === '' ? null : form.routing_strategy,
      affinity_mode: form.affinity_mode === '' ? null : form.affinity_mode,
      affinity_ttl_secs: ttlNum,
      output_guardrails: form.output_guardrails,
    };
    setSaving(true);
    try {
      if (model) {
        await apiPatch(`/api/admin/models/${model.id}`, body);
        toast.success(t('models.toast.updated'));
      } else {
        if (!form.model_id.trim()) {
          setError(t('models.field.modelId') + ' is required');
          setSaving(false);
          return;
        }
        await apiPost('/api/admin/models', {
          ...body,
          model_id: form.model_id.trim(),
        });
        toast.success(t('models.toast.created'));
      }
      await onSaved();
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
            <DialogTitle>{model ? t('models.editTitle') : t('models.createTitle')}</DialogTitle>
            <DialogDescription>{t('models.formHint')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4 py-4">
            {!model && (
              <div className="space-y-2">
                <Label htmlFor="model_id">{t('models.field.modelId')}</Label>
                <Input
                  id="model_id"
                  value={form.model_id}
                  onChange={(e) => setForm({ ...form, model_id: e.target.value })}
                  placeholder="gpt-4o"
                  required
                />
              </div>
            )}
            <div className="space-y-2">
              <Label htmlFor="model_display">{t('models.field.displayName')}</Label>
              <Input
                id="model_display"
                value={form.display_name}
                onChange={(e) => setForm({ ...form, display_name: e.target.value })}
                placeholder={form.model_id}
              />
            </div>
            <p className="text-xs text-muted-foreground">{t('models.weightHint')}</p>
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-2">
                <Label htmlFor="input_weight">{t('models.field.inputWeight')}</Label>
                <Input
                  id="input_weight"
                  value={form.input_weight}
                  onChange={(e) => setForm({ ...form, input_weight: e.target.value })}
                  inputMode="decimal"
                  required
                />
                <CostPreview
                  weight={form.input_weight}
                  basePerToken={pricing?.input_price_per_token}
                  currency={pricing?.currency}
                  side="input"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="output_weight">{t('models.field.outputWeight')}</Label>
                <Input
                  id="output_weight"
                  value={form.output_weight}
                  onChange={(e) => setForm({ ...form, output_weight: e.target.value })}
                  inputMode="decimal"
                  required
                />
                <CostPreview
                  weight={form.output_weight}
                  basePerToken={pricing?.output_price_per_token}
                  currency={pricing?.currency}
                  side="output"
                />
              </div>
            </div>
            <div className="space-y-2">
              <p className="text-xs text-muted-foreground">{t('models.cacheWeightHint')}</p>
              <div className="grid grid-cols-3 gap-3">
                {CACHE_WEIGHTS.map((k) => (
                  <div key={k} className="space-y-2">
                    <Label htmlFor={k}>{t(CACHE_WEIGHT_LABEL[k])}</Label>
                    <Input
                      id={k}
                      value={form[k]}
                      onChange={(e) => setForm({ ...form, [k]: e.target.value })}
                      placeholder={derivedCacheWeight(form.input_weight, k)}
                      inputMode="decimal"
                    />
                  </div>
                ))}
              </div>
            </div>
            {/* Routing strategy + affinity overrides. Empty = inherit
                the global default from system_settings.gateway.*.
                Only useful when an operator wants to diverge from the
                fleet-wide policy for one model. */}
            <div className="space-y-2 border-t pt-4">
              <Label className="text-sm font-medium">{t('models.routingOverrideTitle')}</Label>
              <p className="text-xs text-muted-foreground">{t('models.routingOverrideHint')}</p>
            </div>
            <div className="grid grid-cols-1 gap-3">
              <div className="space-y-2">
                <Label>{t('models.field.routingStrategy')}</Label>
                <Select
                  value={form.routing_strategy === '' ? 'inherit' : form.routing_strategy}
                  onValueChange={(v) =>
                    setForm({
                      ...form,
                      routing_strategy: v === 'inherit' ? '' : (v as RoutingStrategy),
                    })
                  }
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="inherit">{t('models.useGlobalDefault')}</SelectItem>
                    {ROUTING_STRATEGIES.map((s) => (
                      <SelectItem key={s} value={s}>
                        {t(`models.strategy.${s}`)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label>{t('models.field.affinityMode')}</Label>
                  <Select
                    value={form.affinity_mode === '' ? 'inherit' : form.affinity_mode}
                    onValueChange={(v) =>
                      setForm({
                        ...form,
                        affinity_mode: v === 'inherit' ? '' : (v as AffinityMode),
                      })
                    }
                  >
                    <SelectTrigger>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="inherit">{t('models.useGlobalDefault')}</SelectItem>
                      {AFFINITY_MODES.map((m) => (
                        <SelectItem key={m} value={m}>
                          {t(`models.affinity.${m}`)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-2">
                  <Label htmlFor="affinity_ttl">{t('models.field.affinityTtlSecs')}</Label>
                  <Input
                    id="affinity_ttl"
                    value={form.affinity_ttl_secs}
                    onChange={(e) => setForm({ ...form, affinity_ttl_secs: e.target.value })}
                    placeholder={t('models.useGlobalDefault')}
                    inputMode="numeric"
                  />
                </div>
              </div>
            </div>
            {/* Output guardrails — per-model post-flight checks on
                the provider response. Today only "max_length" is
                wired; future variants (JSON schema, toxicity) slot
                in here behind their own add buttons. */}
            <OutputGuardrailsCard
              rules={form.output_guardrails}
              onChange={(next) => setForm({ ...form, output_guardrails: next })}
            />
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
