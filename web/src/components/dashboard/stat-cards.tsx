/**
 * Top-of-page stat tile grid: drag-reorderable 4-up grid + the
 * Suspense-bound TokensCard / CostCard / KeysCard wrappers + the
 * underlying StatCard with embedded shadcn AreaChart sparkline.
 *
 * Each overview endpoint (usage / cost / stats) is fetched as a single
 * stable promise at the page level and consumed here via React 19's
 * `use()`. The component suspends on first render, unblocking a
 * sibling card's render as soon as its own promise resolves — no
 * more page-wide "all three done" gate. ErrorBoundary catches a
 * rejected fetch and falls back to a terminal card variant.
 */

import { Suspense, type ReactNode, use, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Area, AreaChart } from 'recharts';
import { toast } from 'sonner';

import { ErrorBoundary } from '@/components/error-boundary';
import { ChartContainer, type ChartConfig } from '@/components/ui/chart';
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { api, apiPut } from '@/lib/api';

import { fmtCompact, fmtInt, fmtUsd } from './format';
import type {
  CostStats,
  DashboardStats,
  LayoutPayload,
  UsageStats,
} from './types';

const STAT_ORDER_KEY = 'dashboard.stat-order.v1';

/**
 * Reorderable 4-up grid. `cards` is keyed by a stable id — the grid
 * renders children in the persisted order (falling back to the object
 * key order) and lets the user drag a card's handle to rearrange.
 *
 * Order syncs server-side via `/api/dashboard/layout` so it carries
 * across browsers and devices. We still write through to localStorage
 * to avoid a first-paint flash back to default order while the API call
 * is in flight; the API result wins if it differs.
 */
export function StatCardGrid({ cards }: { cards: Record<string, ReactNode> }) {
  const { t } = useTranslation();
  const defaultOrder = useMemo(() => Object.keys(cards), [cards]);

  const mergeOrder = useCallback((saved: unknown): string[] => {
    if (!Array.isArray(saved) || !saved.every((k) => typeof k === 'string')) {
      return defaultOrder;
    }
    // Drop ids that no longer exist (card removed); append new ones at end.
    const known = saved.filter((k) => k in cards);
    for (const k of defaultOrder) if (!known.includes(k)) known.push(k);
    return known;
  }, [cards, defaultOrder]);

  const [order, setOrder] = useState<string[]>(() => {
    try {
      const cached = JSON.parse(localStorage.getItem(STAT_ORDER_KEY) || 'null') as unknown;
      return mergeOrder(cached);
    } catch {
      return defaultOrder;
    }
  });
  const [draggingId, setDraggingId] = useState<string | null>(null);

  // On mount, reconcile with the server-side layout. If the user changed
  // order on another device, this overwrites the local cache. We ignore
  // failures (auth churn, network) — the cached order keeps working.
  useEffect(() => {
    let cancelled = false;
    api<{ name: string; layout_json: LayoutPayload | null }>('/api/dashboard/layout')
      .then((res) => {
        if (cancelled) return;
        const next = mergeOrder(res.layout_json?.stat_order);
        setOrder(next);
        try {
          localStorage.setItem(STAT_ORDER_KEY, JSON.stringify(next));
        } catch {
          // ignore
        }
      })
      .catch(() => {
        /* keep localStorage-backed order */
      });
    return () => {
      cancelled = true;
    };
    // Intentionally only reconcile once per mount — subsequent drags
    // write through to the server, so there's no race to rehydrate.
  }, [mergeOrder]);

  const persist = (next: string[]) => {
    setOrder(next);
    try {
      localStorage.setItem(STAT_ORDER_KEY, JSON.stringify(next));
    } catch {
      // quota / privacy mode — reorder still works for this session.
    }
    // Fire-and-forget PUT; intentionally no debounce because drag events
    // are user-paced and each settled drop is a discrete intent worth
    // persisting immediately. Toast on failure so silent loss is visible.
    apiPut<unknown>('/api/dashboard/layout', {
      name: 'default',
      layout_json: { stat_order: next } satisfies LayoutPayload,
    }).catch((err: unknown) => {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    });
  };

  const reorder = (from: string, to: string) => {
    if (from === to) return;
    const next = order.filter((k) => k !== from);
    const idx = next.indexOf(to);
    next.splice(idx < 0 ? next.length : idx, 0, from);
    persist(next);
  };

  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      {order.map((id) => (
        <div
          key={id}
          draggable
          onDragStart={(e) => {
            setDraggingId(id);
            e.dataTransfer.effectAllowed = 'move';
            // Firefox needs any dataTransfer payload to start a drag.
            e.dataTransfer.setData('text/plain', id);
          }}
          onDragEnd={() => setDraggingId(null)}
          onDragOver={(e) => {
            if (draggingId && draggingId !== id) e.preventDefault();
          }}
          onDrop={(e) => {
            e.preventDefault();
            const from = e.dataTransfer.getData('text/plain');
            if (from) reorder(from, id);
          }}
          className={`transition-opacity ${draggingId === id ? 'opacity-40' : ''} cursor-grab active:cursor-grabbing`}
        >
          {cards[id]}
        </div>
      ))}
    </div>
  );
}

export function SuspendedCard({
  children,
  fallbackLabel,
  chartIndex,
}: {
  children: ReactNode;
  fallbackLabel: string;
  chartIndex: 1 | 2 | 3 | 4 | 5;
}) {
  return (
    <ErrorBoundary
      fallback={
        <StatCard
          label={fallbackLabel}
          value={0}
          format={() => '—'}
          spark={Array(24).fill(0)}
          loading={false}
          chartIndex={chartIndex}
        />
      }
    >
      <Suspense
        fallback={
          <StatCard
            label={fallbackLabel}
            value={0}
            format={() => '…'}
            spark={Array(24).fill(0)}
            loading
            chartIndex={chartIndex}
          />
        }
      >
        {children}
      </Suspense>
    </ErrorBoundary>
  );
}

export function TokensCard({
  promise,
  locale,
  label,
}: {
  promise: Promise<UsageStats>;
  locale: string;
  label: string;
}) {
  const usage = use(promise);
  return (
    <StatCard
      label={label}
      value={usage.total_tokens}
      format={(v) => fmtCompact(v, locale)}
      spark={usage.tokens_buckets}
      loading={false}
      chartIndex={1}
      prev={usage.prev_total_tokens}
    />
  );
}

export function CostCard({
  promise,
  locale,
  label,
}: {
  promise: Promise<CostStats>;
  locale: string;
  label: string;
}) {
  const cost = use(promise);
  // Dashboard uses Number for the visual card / sparkline — display
  // precision is fine. The backend keeps Decimal end-to-end for any
  // consumer that needs billing-grade numbers (costs page, chargeback
  // CSV); this is deliberately the one place the frontend downgrades.
  return (
    <StatCard
      label={label}
      value={Number(cost.total_cost)}
      format={(v) => fmtUsd(v, locale)}
      delta={cost.budget_usage_pct != null ? `${cost.budget_usage_pct.toFixed(1)}%` : undefined}
      spark={cost.cost_buckets.map(Number)}
      loading={false}
      chartIndex={2}
      prev={cost.prev_total_cost !== undefined ? Number(cost.prev_total_cost) : undefined}
    />
  );
}

export function KeysCard({
  promise,
  locale,
  label,
}: {
  promise: Promise<DashboardStats>;
  locale: string;
  label: string;
}) {
  const stats = use(promise);
  return (
    <StatCard
      label={label}
      value={stats.active_api_keys}
      format={(v) => fmtInt(Math.round(v), locale)}
      spark={stats.active_keys_buckets}
      loading={false}
      chartIndex={3}
      prev={stats.prev_active_api_keys}
    />
  );
}

// ----------------------------------------------------------------------------
// Stat card with embedded shadcn AreaChart sparkline
// ----------------------------------------------------------------------------

interface StatCardProps {
  label: string;
  value: number;
  format: (v: number) => string;
  delta?: string;
  spark: number[];
  loading: boolean;
  chartIndex: 1 | 2 | 3 | 4 | 5;
  /**
   * Comparison value from the previous-period query (`?compare=true`).
   * When present, the card renders a small chip with ↑/↓ + percent
   * change. Distinct from `delta` (which is the budget-vs-spend
   * percentage on the cost card) so both can show simultaneously.
   */
  prev?: number;
}

function useCounter(target: number, duration = 1200) {
  const [value, setValue] = useState(target);
  const fromRef = useRef(target);
  useEffect(() => {
    const from = fromRef.current;
    if (from === target) return;
    const start = performance.now();
    let raf = 0;
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / duration);
      const eased = 1 - Math.pow(1 - t, 3);
      const v = from + (target - from) * eased;
      setValue(v);
      if (t < 1) raf = requestAnimationFrame(tick);
      else fromRef.current = target;
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target, duration]);
  return value;
}

export function StatCard({ label, value, format, delta, spark, loading, chartIndex, prev }: StatCardProps) {
  const { t } = useTranslation();
  const animated = useCounter(value);
  const data = useMemo(() => spark.map((v, i) => ({ i, v })), [spark]);
  const config = {
    v: { label, color: `var(--chart-${chartIndex})` },
  } satisfies ChartConfig;

  // Compute percent change vs previous period. Three edge cases:
  //   prev undefined    → compare mode is off; render nothing
  //   prev === 0 && value === 0 → no change; render flat
  //   prev === 0 && value > 0   → infinite growth from zero; render
  //                                "new" instead of a misleading 0%
  let cmpChip: { text: string; tone: 'pos' | 'neg' | 'flat' | 'new' } | null = null;
  if (prev !== undefined) {
    if (prev === 0 && value === 0) {
      cmpChip = { text: '0%', tone: 'flat' };
    } else if (prev === 0) {
      cmpChip = { text: 'new', tone: 'pos' };
    } else {
      const pct = ((value - prev) / prev) * 100;
      const sign = pct > 0 ? '↑' : pct < 0 ? '↓' : '';
      cmpChip = {
        text: `${sign}${Math.abs(pct).toFixed(1)}%`,
        tone: pct > 0 ? 'pos' : pct < 0 ? 'neg' : 'flat',
      };
    }
  }
  const cmpToneClass: Record<'pos' | 'neg' | 'flat' | 'new', string> = {
    pos: 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-400',
    neg: 'bg-destructive/15 text-destructive',
    flat: 'bg-muted text-muted-foreground',
    new: 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-400',
  };

  return (
    <Card size="sm">
      <CardHeader>
        <CardDescription>{label}</CardDescription>
        <CardTitle className="font-mono text-2xl tabular-nums tracking-tight">
          {loading ? '…' : format(animated)}
        </CardTitle>
        {(delta || cmpChip) && (
          <CardAction>
            <div className="flex items-center gap-1">
              {cmpChip && (
                <span
                  className={`rounded px-1.5 py-0.5 font-mono text-[10px] font-medium ${cmpToneClass[cmpChip.tone]}`}
                  title={prev !== undefined ? `${t('dashboard.previousValuePrefix')}: ${format(prev)}` : undefined}
                >
                  {cmpChip.text}
                </span>
              )}
              {delta && (
                <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] font-medium text-muted-foreground">
                  {delta}
                </span>
              )}
            </div>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="px-0 pb-0">
        <ChartContainer config={config} className="aspect-auto h-14 w-full">
          <AreaChart data={data} margin={{ top: 0, right: 0, bottom: 0, left: 0 }}>
            <defs>
              <linearGradient id={`stat-fill-${chartIndex}`} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor="var(--color-v)" stopOpacity={0.4} />
                <stop offset="100%" stopColor="var(--color-v)" stopOpacity={0} />
              </linearGradient>
            </defs>
            <Area
              dataKey="v"
              // `monotone` clamps the spline so it never overshoots
              // a data point — critical for sparse RPM-style data
              // where `natural` would dip the curve below 0 at the
              // troughs between peaks, drawing the line under the
              // x-axis baseline.
              type="monotone"
              stroke="var(--color-v)"
              strokeWidth={1.6}
              fill={`url(#stat-fill-${chartIndex})`}
              isAnimationActive={false}
            />
          </AreaChart>
        </ChartContainer>
      </CardContent>
    </Card>
  );
}
