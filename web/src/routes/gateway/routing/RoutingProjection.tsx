import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Loader2, Sparkles, Scale } from 'lucide-react';
import { toast } from 'sonner';
import { api, apiPatch } from '@/lib/api';
import type { RoutingProjectionResponse } from '../models';

interface Props {
  modelId: string;
  /// Pricing currency for the cost preview line. Falls back to USD.
  currency?: string;
  /// Whether the wizard is in manual mode (admin sets ratios). Match-auto
  /// and even-split buttons only make sense in manual mode.
  manualMode: boolean;
  /// Disable controls when admin lacks `models:write`.
  disabled?: boolean;
  /// Trigger a refresh of the routes table after a successful weight
  /// update (match-auto / even-split). Caller owns route fetching.
  onWeightsChanged: () => void;
}

/// Bottom strip of the routing section: shows the projected cost-per-1M
/// for the *current* config, plus convenience buttons that re-balance
/// weights when the admin is in manual mode:
///   * **Even split** — every route gets the same weight.
///   * **Match auto** — copy what `latency_cost` (or current strategy)
///     would have picked, giving the admin a smart starting point.
export function RoutingProjection({
  modelId,
  currency = 'USD',
  manualMode,
  disabled,
  onWeightsChanged,
}: Props) {
  const { t } = useTranslation();
  const [data, setData] = useState<RoutingProjectionResponse | null>(null);
  const [acting, setActing] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const res = await api<RoutingProjectionResponse>(
          `/api/admin/models/${encodeURIComponent(modelId)}/routing-projection`,
        );
        if (!cancelled) setData(res);
      } catch {
        // Silent — projection is informational.
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [modelId]);

  const expectedCost = data?.current?.expected_cost_per_1m_tokens;
  const costLabel =
    expectedCost == null
      ? t('models.routing.expectedCostUnknown')
      : t('models.routing.expectedCostPer1m', {
          amount: expectedCost.toFixed(4),
          currency,
        });

  const matchAuto = async () => {
    if (!data) return;
    // Use the auto projection's expected_pct as integer weights.
    // Multiply by 100 and round so admin sees clean numbers (67/22/11),
    // not opaque floats. Routes the auto projection didn't enumerate
    // are left as-is.
    const updates = data.auto.entries.map((e) => ({
      id: e.route_id,
      weight: Math.max(0, Math.round(e.expected_pct)),
    }));
    setActing(true);
    try {
      await apiPatch('/api/admin/model-routes/batch-weights', { updates });
      onWeightsChanged();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Match auto failed');
    } finally {
      setActing(false);
    }
  };

  const evenSplit = async () => {
    if (!data) return;
    const updates = data.current.entries.map((e) => ({
      id: e.route_id,
      weight: 100,
    }));
    setActing(true);
    try {
      await apiPatch('/api/admin/model-routes/batch-weights', { updates });
      onWeightsChanged();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Even split failed');
    } finally {
      setActing(false);
    }
  };

  return (
    <div className="flex items-center justify-between gap-3 rounded-md border bg-muted/10 px-3 py-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="text-muted-foreground">
          {t('models.routing.expectedCostLabel')}:
        </span>
        <span className="font-mono">{costLabel}</span>
      </div>
      {manualMode && (
        <div className="flex items-center gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            onClick={evenSplit}
            disabled={disabled || acting || !data}
            title={t('models.routing.evenSplit')}
          >
            <Scale className="mr-1 h-3 w-3" />
            {t('models.routing.evenSplit')}
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            onClick={matchAuto}
            disabled={disabled || acting || !data}
            title={t('models.routing.matchAutoHint')}
          >
            {acting ? (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            ) : (
              <Sparkles className="mr-1 h-3 w-3" />
            )}
            {t('models.routing.matchAuto')}
          </Button>
        </div>
      )}
    </div>
  );
}
