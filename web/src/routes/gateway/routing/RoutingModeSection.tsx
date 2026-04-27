import { useTranslation } from 'react-i18next';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Sparkles, SlidersHorizontal } from 'lucide-react';
import {
  AUTO_TARGETS,
  type RoutingStrategy,
} from '../models';

interface Props {
  /// Per-model override. `null` = inherit global default.
  modelStrategy: RoutingStrategy | null;
  /// What the global default resolves to today.
  globalStrategy: RoutingStrategy;
  /// Disable controls when admin lacks `models:write`.
  disabled?: boolean;
  /// Fired when admin picks a new strategy. `null` = clear override
  /// (inherit global). Caller persists via PATCH /api/admin/models/{id}.
  onChange: (next: RoutingStrategy | null) => void;
}

/// The "auto / manual" mode picker that sits at the top of a model's
/// route list. Mode = derived from `modelStrategy` (or `globalStrategy`
/// if model has no override): "manual" iff `weighted`, else "auto" with
/// the strategy as the optimization target.
///
/// Two state transitions admin can make:
///  - Toggle to manual ⇒ persist `weighted`
///  - Toggle to auto ⇒ persist a default auto strategy (`latency_cost`)
///  - Within auto, change target ⇒ persist that target
///
/// Picking a strategy that matches the global default still stores the
/// override (so admin's intent doesn't silently track a global change).
/// We surface "current source" so they can tell whether they're seeing
/// inherited behaviour or their own pick.
export function RoutingModeSection({
  modelStrategy,
  globalStrategy,
  disabled,
  onChange,
}: Props) {
  const { t } = useTranslation();

  const effective: RoutingStrategy = modelStrategy ?? globalStrategy;
  const isManual = effective === 'weighted';
  const sourceLabel = modelStrategy
    ? t('models.routing.sourceModel')
    : t('models.routing.sourceGlobal');

  const setMode = (mode: 'auto' | 'manual') => {
    if (mode === 'manual') {
      onChange('weighted');
    } else {
      // Default auto target = latency_cost. Admin can change it via
      // the sub-picker below.
      onChange('latency_cost');
    }
  };

  return (
    <div className="rounded-md border bg-muted/20 p-3 space-y-3">
      <div className="grid grid-cols-2 gap-2">
        <button
          type="button"
          disabled={disabled}
          onClick={() => setMode('auto')}
          className={`flex items-start gap-2 rounded border p-3 text-left transition ${
            !isManual
              ? 'border-primary bg-primary/10'
              : 'border-transparent hover:border-muted-foreground/30'
          } ${disabled ? 'opacity-50 cursor-not-allowed' : ''}`}
        >
          <Sparkles
            className={`h-4 w-4 mt-0.5 shrink-0 ${
              !isManual ? 'text-primary' : 'text-muted-foreground'
            }`}
          />
          <div className="space-y-0.5 min-w-0">
            <div className="text-sm font-medium">
              {t('models.routing.modeAuto')}
            </div>
            <div className="text-[11px] text-muted-foreground leading-snug">
              {t('models.routing.modeAutoHint')}
            </div>
          </div>
        </button>
        <button
          type="button"
          disabled={disabled}
          onClick={() => setMode('manual')}
          className={`flex items-start gap-2 rounded border p-3 text-left transition ${
            isManual
              ? 'border-primary bg-primary/10'
              : 'border-transparent hover:border-muted-foreground/30'
          } ${disabled ? 'opacity-50 cursor-not-allowed' : ''}`}
        >
          <SlidersHorizontal
            className={`h-4 w-4 mt-0.5 shrink-0 ${
              isManual ? 'text-primary' : 'text-muted-foreground'
            }`}
          />
          <div className="space-y-0.5 min-w-0">
            <div className="text-sm font-medium">
              {t('models.routing.modeManual')}
            </div>
            <div className="text-[11px] text-muted-foreground leading-snug">
              {t('models.routing.modeManualHint')}
            </div>
          </div>
        </button>
      </div>
      {!isManual && (
        <div className="flex items-center gap-2 pt-1">
          <Label className="text-xs whitespace-nowrap">
            {t('models.routing.autoTargetLabel')}
          </Label>
          <Select
            value={effective}
            onValueChange={(v) => onChange(v as RoutingStrategy)}
            disabled={disabled}
          >
            <SelectTrigger className="h-7 text-xs w-56">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {AUTO_TARGETS.map((s) => (
                <SelectItem key={s} value={s}>
                  {t(`models.strategy.${s}`)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
      <div className="text-[11px] text-muted-foreground">
        {t('models.routing.currentSource', { source: sourceLabel })}
        {' · '}
        <span className="font-mono">{t(`models.strategy.${effective}`)}</span>
      </div>
    </div>
  );
}
