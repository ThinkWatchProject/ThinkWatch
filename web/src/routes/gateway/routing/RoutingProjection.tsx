import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/lib/api';
import type { RoutingProjectionResponse } from '../models';

interface Props {
  modelId: string;
  /// Pricing currency for the cost preview line. Falls back to USD.
  currency?: string;
  /// Caller-controlled cache key — bump it when routes change so the
  /// projection refetches without unmounting (which would flash "—"
  /// while the request is in flight).
  refreshKey?: string | number;
}

/// Bottom strip of the routing section. One-line cost projection for
/// the current weights + strategy. Reset-to-even-split lives on the
/// TrafficBar itself; we used to also offer "match auto" but admins
/// can just toggle to auto mode if that's what they want.
export function RoutingProjection({ modelId, currency = 'USD', refreshKey }: Props) {
  const { t } = useTranslation();
  const [data, setData] = useState<RoutingProjectionResponse | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const res = await api<RoutingProjectionResponse>(
          `/api/admin/models/${encodeURIComponent(modelId)}/routing-projection`,
        );
        // Keep the previous `data` showing while the new fetch lands —
        // the component never resets to null on refresh, so the cost
        // line doesn't flash "—" between drags.
        if (!cancelled) setData(res);
      } catch {
        // Silent — projection is informational.
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [modelId, refreshKey]);

  const expectedCost = data?.current?.expected_cost_per_1m_tokens;
  const costLabel =
    expectedCost == null
      ? t('models.routing.expectedCostUnknown')
      : t('models.routing.expectedCostPer1m', {
          amount: expectedCost.toFixed(4),
          currency,
        });

  return (
    <div className="flex items-center gap-2 rounded-md border bg-muted/10 px-3 py-2 text-xs">
      <span className="text-muted-foreground">
        {t('models.routing.expectedCostLabel')}:
      </span>
      <span className="font-mono">{costLabel}</span>
    </div>
  );
}
