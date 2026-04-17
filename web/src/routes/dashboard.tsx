import {
  memo,
  Suspense,
  use,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from 'react';
import { ErrorBoundary } from '@/components/error-boundary';
import { useTranslation } from 'react-i18next';
import { Inbox } from 'lucide-react';
import { Area, AreaChart, CartesianGrid, ReferenceLine, YAxis } from 'recharts';
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { api, apiPut } from '@/lib/api';
import { StatusIndicator } from '@/components/ui/status-indicator';
import { ServiceLogo } from '@/components/ui/service-logo';
import {
  DashboardLiveSchema,
  WsTicketSchema,
  type DashboardLive,
  type LiveLogRow,
  type ProviderHealth,
} from '@/lib/schemas';
import { toast } from 'sonner';

interface DashboardStats {
  total_requests: number;
  active_providers: number;
  active_api_keys: number;
  connected_mcp_servers: number;
  active_keys_buckets: number[];
  range: string;
  prev_total_requests?: number;
  prev_active_api_keys?: number;
}

interface UsageStats {
  total_tokens: number;
  total_requests: number;
  tokens_buckets: number[];
  range: string;
  prev_total_tokens?: number;
  prev_total_requests?: number;
}

interface CostStats {
  total_cost: number;
  budget_usage_pct: number | null;
  cost_buckets: number[];
  range: string;
  total_cost_mtd: number;
  prev_total_cost?: number;
}

type TimeRange = '24h' | '7d' | '30d';
const TIME_RANGES: readonly TimeRange[] = ['24h', '7d', '30d'] as const;


// Locale-aware formatters — react to i18next language changes by reading
// the current language at format time. We don't cache the formatter
// instances aggressively because the language only changes on user action,
// and Intl.NumberFormat construction is fast.
function fmtCompact(v: number, locale: string): string {
  return new Intl.NumberFormat(locale, { notation: 'compact', maximumFractionDigits: 1 }).format(v);
}
function fmtUsd(v: number, locale: string): string {
  return new Intl.NumberFormat(locale, {
    style: 'currency',
    currency: 'USD',
    maximumFractionDigits: 2,
  }).format(v);
}
function fmtInt(v: number, locale: string): string {
  return new Intl.NumberFormat(locale).format(v);
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



// ----------------------------------------------------------------------------
// Live snapshot via WebSocket. Falls back to a one-shot HTTP fetch if WS
// can't connect (e.g. behind a proxy that doesn't speak the upgrade).
// ----------------------------------------------------------------------------

function useLiveDashboard() {
  const [live, setLive] = useState<DashboardLive | null>(null);
  const [connected, setConnected] = useState(false);
  // Ref mirror so the WS callbacks can read "have we ever received data?"
  // without capturing a stale closure.
  const liveRef = useRef<DashboardLive | null>(null);
  liveRef.current = live;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;
    let backoff = 1000;

    // Detach all handlers from a WebSocket before closing it so the
    // close event can't trigger a reconnect we don't want — used both
    // when the tab goes hidden and when the effect tears down.
    const closeQuietly = (w: WebSocket | null) => {
      if (!w) return;
      w.onopen = null;
      w.onmessage = null;
      w.onerror = null;
      w.onclose = null;
      try {
        w.close();
      } catch {
        // ignore — best-effort cleanup
      }
    };

    const connect = async () => {
      if (cancelled) return;
      // If a previous socket is still around (e.g. an errored one
      // whose onclose hasn't fired yet), detach it so its delayed
      // close can't queue another reconnect on top of this one.
      closeQuietly(ws);
      ws = null;
      // Auth tokens live in HttpOnly cookies now, so the page JS
      // can't pre-check "are we logged in". We just try to mint
      // the WS ticket — if the user isn't authenticated the api
      // client gets a 401 and routes them through the standard
      // refresh-then-redirect flow.
      // Mint a single-use ticket via authenticated POST. The ticket is
      // bound to the user_id and expires in 30s; the WS endpoint atomically
      // consumes it. This keeps the JWT out of the WS URL (which would
      // otherwise leak through access logs, browser history, and Referer
      // headers).
      let ticket: string;
      try {
        const res = await api<{ ticket: string }>('/api/dashboard/ws-ticket', {
          method: 'POST',
          schema: WsTicketSchema,
        });
        ticket = res.ticket;
      } catch {
        scheduleReconnect();
        return;
      }
      if (cancelled) return;

      const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
      const apiBase = import.meta.env.VITE_API_BASE ?? '';
      const httpUrl = new URL(
        `${apiBase}/api/dashboard/ws?ticket=${encodeURIComponent(ticket)}`,
        window.location.origin,
      );
      const wsUrl = `${proto}://${httpUrl.host}${httpUrl.pathname}${httpUrl.search}`;

      try {
        ws = new WebSocket(wsUrl);
      } catch {
        scheduleReconnect();
        return;
      }

      ws.onopen = () => {
        backoff = 1000;
        setConnected(true);
      };
      ws.onmessage = (ev) => {
        try {
          const payload = JSON.parse(ev.data) as DashboardLive;
          setLive(payload);
        } catch (err) {
          // Surface parse failures so they're visible in devtools.
          console.error('dashboard ws parse failed', err, ev.data);
        }
      };
      ws.onerror = () => {
        // Surface state, the close handler will trigger reconnect.
        setConnected(false);
      };
      ws.onclose = () => {
        setConnected(false);
        // First close — try a one-shot HTTP fetch so the user sees data
        // immediately even if WS is unavailable. Reads via ref so it
        // sees the latest state, not a stale closure capture.
        if (liveRef.current === null) {
          api<DashboardLive>('/api/dashboard/live', { schema: DashboardLiveSchema })
            .then(setLive)
            .catch((err) => {
              // The WS closed and HTTP fallback also failed — the user
              // will see the "disconnected" indicator but should also
              // know data is missing. Log to console for debugging.
              // Don't toast here: the reconnect loop will retry shortly
              // and a toast per close would spam the UI.
              console.warn('[dashboard] live fallback fetch failed:', err);
            });
        }
        scheduleReconnect();
      };
    };

    const scheduleReconnect = () => {
      if (cancelled) return;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(() => {
        void connect();
      }, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };

    void connect();

    const onVis = () => {
      if (document.hidden) {
        // Detach handlers BEFORE closing — otherwise the queued
        // onclose would call scheduleReconnect and we'd silently
        // reconnect in the background while the tab is hidden.
        closeQuietly(ws);
        ws = null;
        if (reconnectTimer) {
          clearTimeout(reconnectTimer);
          reconnectTimer = null;
        }
      } else if (!ws || ws.readyState === WebSocket.CLOSED) {
        void connect();
      }
    };
    document.addEventListener('visibilitychange', onVis);

    return () => {
      cancelled = true;
      document.removeEventListener('visibilitychange', onVis);
      if (reconnectTimer) clearTimeout(reconnectTimer);
      closeQuietly(ws);
      ws = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { live, connected };
}

// ----------------------------------------------------------------------------
// Stat-card grid — native HTML5 drag-reorder + localStorage persistence
// ----------------------------------------------------------------------------

const STAT_ORDER_KEY = 'dashboard.stat-order.v1';

interface LayoutPayload {
  stat_order?: string[];
}

/// Reorderable 4-up grid. `cards` is keyed by a stable id — the grid
/// renders children in the persisted order (falling back to the object
/// key order) and lets the user drag a card's handle to rearrange.
///
/// Order syncs server-side via `/api/dashboard/layout` so it carries
/// across browsers and devices. We still write through to localStorage
/// to avoid a first-paint flash back to default order while the API call
/// is in flight; the API result wins if it differs.
function StatCardGrid({ cards }: { cards: Record<string, ReactNode> }) {
  const defaultOrder = useMemo(() => Object.keys(cards), [cards]);

  const mergeOrder = (saved: unknown): string[] => {
    if (!Array.isArray(saved) || !saved.every((k) => typeof k === 'string')) {
      return defaultOrder;
    }
    // Drop ids that no longer exist (card removed); append new ones at end.
    const known = saved.filter((k) => k in cards);
    for (const k of defaultOrder) if (!known.includes(k)) known.push(k);
    return known;
  };

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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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
      toast.error(err instanceof Error ? err.message : 'Failed to save layout');
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

// ----------------------------------------------------------------------------
// Suspense-bound card wrappers
//
// Each overview endpoint (usage / cost / stats) is fetched as a single
// stable promise at the page level and consumed here via React 19's
// use(). The component suspends on first render, unblocking a sibling
// card's render as soon as its own promise resolves — no more
// page-wide "all three done" gate. ErrorBoundary catches a rejected
// fetch and falls back to a terminal card variant.
// ----------------------------------------------------------------------------

function SuspendedCard({
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

function TokensCard({
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

function CostCard({
  promise,
  locale,
  label,
}: {
  promise: Promise<CostStats>;
  locale: string;
  label: string;
}) {
  const cost = use(promise);
  return (
    <StatCard
      label={label}
      value={cost.total_cost}
      format={(v) => fmtUsd(v, locale)}
      delta={cost.budget_usage_pct != null ? `${cost.budget_usage_pct.toFixed(1)}%` : undefined}
      spark={cost.cost_buckets}
      loading={false}
      chartIndex={2}
      prev={cost.prev_total_cost}
    />
  );
}

function KeysCard({
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
  /// Comparison value from the previous-period query (`?compare=true`).
  /// When present, the card renders a small chip with ↑/↓ + percent
  /// change. Distinct from `delta` (which is the budget-vs-spend
  /// percentage on the cost card) so both can show simultaneously.
  prev?: number;
}

function StatCard({ label, value, format, delta, spark, loading, chartIndex, prev }: StatCardProps) {
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
                  title={prev !== undefined ? `prev: ${format(prev)}` : undefined}
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
              type="natural"
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

// ----------------------------------------------------------------------------
// Helpers for the live log feed
// ----------------------------------------------------------------------------

function fmtTime(d: Date): string {
  return [d.getHours(), d.getMinutes(), d.getSeconds()]
    .map((n) => String(n).padStart(2, '0'))
    .join(':');
}

function statusBadgeClass(kind: 'api' | 'mcp', status: string): string {
  if (kind === 'api') {
    const code = parseInt(status, 10);
    if (!Number.isFinite(code) || code === 0) return 'bg-muted text-muted-foreground';
    if (code >= 500 || (code >= 400 && code !== 429))
      return 'bg-destructive/10 text-destructive';
    if (code === 429) return 'bg-muted text-foreground';
    return 'bg-primary/10 text-primary';
  }
  // MCP statuses are strings: success / error / failed / timeout / ...
  const s = status.toLowerCase();
  if (!s) return 'bg-muted text-muted-foreground';
  if (s === 'success' || s === 'ok') return 'bg-primary/10 text-primary';
  if (s === 'timeout' || s === 'rate_limited') return 'bg-muted text-foreground';
  return 'bg-destructive/10 text-destructive';
}

function shortId(s: string | null, n = 8): string {
  if (!s) return '—';
  return s.length > n ? s.slice(0, n) : s;
}

function kindBadgeClass(kind: string): string {
  // "api" (log row) and "ai" (provider) are both AI gateway things → primary tint
  // "mcp" → muted
  return kind === 'mcp'
    ? 'border-border bg-muted text-foreground'
    : 'border-primary/30 bg-primary/10 text-primary';
}

// ----------------------------------------------------------------------------
// Page
// ----------------------------------------------------------------------------

export function DashboardPage() {
  const { t, i18n } = useTranslation();
  // Map i18next language code → BCP 47 locale for Intl.NumberFormat.
  const locale = i18n.language === 'zh' ? 'zh-CN' : 'en-US';
  const { live, connected } = useLiveDashboard();

  // Global time-range filter. Selecting a different range remounts the
  // three Suspense cards (see `key={range}` below), which mints fresh
  // promises for the new window.
  const [range, setRange] = useState<TimeRange>(() => {
    const cached = typeof window !== 'undefined' ? window.localStorage.getItem('dashboard.range.v1') : null;
    return cached && (TIME_RANGES as readonly string[]).includes(cached) ? (cached as TimeRange) : '24h';
  });
  useEffect(() => {
    try {
      window.localStorage.setItem('dashboard.range.v1', range);
    } catch {
      // ignore
    }
  }, [range]);

  // "Compare to previous" toggle. When on, the three stats endpoints
  // are queried with `?compare=true` and each card renders a delta
  // chip vs the immediately-preceding window of the same length.
  const [compare, setCompare] = useState<boolean>(() => {
    return typeof window !== 'undefined'
      && window.localStorage.getItem('dashboard.compare.v1') === '1';
  });
  useEffect(() => {
    try {
      window.localStorage.setItem('dashboard.compare.v1', compare ? '1' : '0');
    } catch {
      // ignore
    }
  }, [compare]);

  // Each of the three overview endpoints becomes its own stable promise,
  // consumed by a dedicated <Suspense>-wrapped child via React 19's
  // use(). A card renders as soon as *its* endpoint resolves — the slow
  // one no longer blocks the other two. useMemo keyed on `range` +
  // `compare` makes the promise identity stable across re-renders but
  // refreshes whenever either control changes.
  const compareQs = compare ? '&compare=true' : '';
  const statsPromise = useMemo(
    () => api<DashboardStats>(`/api/dashboard/stats?range=${range}${compareQs}`),
    [range, compareQs],
  );
  const usagePromise = useMemo(
    () => api<UsageStats>(`/api/analytics/usage/stats?range=${range}${compareQs}`),
    [range, compareQs],
  );
  const costPromise = useMemo(
    () => api<CostStats>(`/api/analytics/costs/stats?range=${range}${compareQs}`),
    [range, compareQs],
  );

  // Toast-on-rejection is still useful — keep the "something failed"
  // signal but out-of-band from the render path (ErrorBoundaries below
  // catch the actual throw and render an error affordance).
  useEffect(() => {
    const pairs: Array<[string, Promise<unknown>]> = [
      ['stats', statsPromise],
      ['usage', usagePromise],
      ['cost', costPromise],
    ];
    Promise.allSettled(pairs.map(([, p]) => p)).then((results) => {
      const failed = results
        .map((r, i) => (r.status === 'rejected' ? pairs[i][0] : null))
        .filter((x): x is string => !!x);
      if (failed.length > 0) {
        toast.error(t('dashboard.loadFailed', { what: failed.join(', ') }));
      }
    });
  }, [t, statsPromise, usagePromise, costPromise]);

  const rpmSpark = useMemo(() => live?.rpm_buckets ?? Array(24).fill(0), [live]);
  const currentRpm = live?.rpm_buckets?.[live.rpm_buckets.length - 1] ?? 0;

  // Upstream-health filter (all / ai / mcp). Counts come from the live
  // snapshot so the tab pills always show the current per-kind totals.
  const [providerFilter, setProviderFilter] = useState<ProviderFilter>('all');
  const allProviders = live?.providers ?? [];
  const providerCounts = useMemo(
    () => ({
      all: allProviders.length,
      ai: allProviders.filter((p) => p.kind === 'ai').length,
      mcp: allProviders.filter((p) => p.kind === 'mcp').length,
    }),
    [allProviders],
  );
  const filteredProviders = useMemo(() => {
    if (live === null) return null;
    if (providerFilter === 'all') return allProviders;
    return allProviders.filter((p) => p.kind === providerFilter);
  }, [live, allProviders, providerFilter]);

  return (
    // Full-viewport layout — the entire dashboard fits on one screen with
    // internal scrolling on the lists, never a page-level scrollbar.
    // `min-h-0` cascades through the nested flex containers so the bottom
    // grid can actually shrink to fit.
    <div className="flex h-full min-h-0 flex-1 flex-col gap-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('dashboard.title')}</h1>
          <p className="text-sm text-muted-foreground">{t('dashboard.subtitle')}</p>
        </div>
        <div className="flex items-center gap-4">
          {/* Global time-range filter — drives all three overview queries. */}
          <div
            role="radiogroup"
            aria-label={t('dashboard.rangeLabel')}
            className="inline-flex items-center gap-0.5 rounded-md border bg-muted/30 p-0.5 text-xs"
          >
            {TIME_RANGES.map((r) => (
              <button
                key={r}
                type="button"
                role="radio"
                aria-checked={range === r}
                onClick={() => setRange(r)}
                className={`rounded px-2 py-1 font-medium transition-colors ${
                  range === r
                    ? 'bg-background text-foreground shadow-sm'
                    : 'text-muted-foreground hover:text-foreground'
                }`}
              >
                {t(`dashboard.range.${r}`)}
              </button>
            ))}
          </div>
          <button
            type="button"
            role="switch"
            aria-checked={compare}
            onClick={() => setCompare((v) => !v)}
            className={`rounded-md border px-2 py-1 text-xs font-medium transition-colors ${
              compare
                ? 'border-primary/60 bg-primary/10 text-primary'
                : 'border-border bg-muted/30 text-muted-foreground hover:text-foreground'
            }`}
            title={t('dashboard.compareHint')}
          >
            {t('dashboard.compare')}
          </button>
          <div
            className="flex items-center gap-1.5 text-[10px] uppercase tracking-wider text-muted-foreground"
            aria-live="polite"
          >
            <span
              aria-hidden="true"
              className={`h-1.5 w-1.5 rounded-full ${
                connected ? 'animate-pulse bg-foreground' : 'bg-muted-foreground'
              }`}
            />
            {connected ? t('dashboard.live') : t('dashboard.reconnecting')}
          </div>
        </div>
      </div>

      <Section eyebrow={t('dashboard.overviewEyebrow')} className="shrink-0">
        <StatCardGrid
          cards={{
            tokens: (
              <SuspendedCard fallbackLabel={t('dashboard.tokensUsedToday')} chartIndex={1}>
                <TokensCard
                  promise={usagePromise}
                  locale={locale}
                  label={t('dashboard.tokensUsedToday')}
                />
              </SuspendedCard>
            ),
            cost: (
              <SuspendedCard fallbackLabel={t('dashboard.costMtd')} chartIndex={2}>
                <CostCard
                  promise={costPromise}
                  locale={locale}
                  label={t('dashboard.costMtd')}
                />
              </SuspendedCard>
            ),
            keys: (
              <SuspendedCard fallbackLabel={t('dashboard.activeApiKeys')} chartIndex={3}>
                <KeysCard
                  promise={statsPromise}
                  locale={locale}
                  label={t('dashboard.activeApiKeys')}
                />
              </SuspendedCard>
            ),
            rpm: (
              // RPM is already streaming over the WS channel — no
              // Suspense needed, but it reads `live?.rpm_buckets` which
              // stays null-safe until the first frame arrives.
              <StatCard
                label={t('dashboard.requestsPerMin')}
                value={currentRpm}
                format={(v) => fmtInt(Math.round(v), locale)}
                delta={t('dashboard.live')}
                spark={rpmSpark}
                loading={!live}
                chartIndex={4}
              />
            ),
          }}
        />
      </Section>

      {/* Bottom region — flex-1 fills the rest of the viewport. The inner
          panels use `min-h-0 flex-1` + internal `overflow-y-auto` so they
          shrink instead of pushing the page beyond one screen. */}
      <div className="grid min-h-0 flex-1 gap-4 lg:grid-cols-[1.4fr_1fr]">
        <Section
          eyebrow={t('dashboard.logsEyebrow')}
          className="flex min-h-0 flex-col"
        >
          <LiveLogPanel rows={live?.recent_logs ?? null} />
        </Section>

        <div className="flex min-h-0 flex-col gap-4">
          {/* Upstream health takes whatever vertical space is left after
              the (compact, fixed) RPM panel below. */}
          <Section
            eyebrow={t('dashboard.providerHealth')}
            className="flex min-h-0 flex-1 flex-col"
            action={
              <ProviderFilterTabs
                value={providerFilter}
                onChange={setProviderFilter}
                counts={providerCounts}
              />
            }
          >
            <ProviderHealthPanel rows={filteredProviders} />
          </Section>
          <Section eyebrow={t('dashboard.requestRate')} className="shrink-0">
            <RpmWindowPanel
              buckets={live?.rpm_buckets ?? null}
              maxRpm={live?.max_rpm_limit ?? null}
            />
          </Section>
        </div>
      </div>
    </div>
  );
}

// Tiny wrapper that renders a section eyebrow above its child panel. The
// child fills the remaining vertical space when the parent flexes (so the
// internal scroll lists can shrink to fit). An optional `action` slot is
// rendered right-aligned next to the eyebrow (e.g. tab switchers).
function Section({
  eyebrow,
  action,
  className,
  children,
}: {
  eyebrow: string;
  action?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`flex min-h-0 flex-col ${className ?? ''}`}>
      <div className="mb-2 flex shrink-0 items-center justify-between gap-3">
        <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-muted-foreground">
          {eyebrow}
        </div>
        {action}
      </div>
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}

// Tab switcher used in the upstream-health eyebrow. Three options:
// all / ai / mcp. Designed to be visually quiet — uppercase tracking-wider
// labels matching the eyebrow itself.
type ProviderFilter = 'all' | 'ai' | 'mcp';

function ProviderFilterTabs({
  value,
  onChange,
  counts,
}: {
  value: ProviderFilter;
  onChange: (v: ProviderFilter) => void;
  counts: { all: number; ai: number; mcp: number };
}) {
  const tabs: { key: ProviderFilter; label: string }[] = [
    { key: 'all', label: 'all' },
    { key: 'ai', label: 'ai' },
    { key: 'mcp', label: 'mcp' },
  ];
  // Arrow-key navigation between tabs (W3C tablist pattern). Left/Right
  // wrap around; Home/End jump to the first/last tab.
  const onKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    const idx = tabs.findIndex((t) => t.key === value);
    if (idx < 0) return;
    let next = idx;
    if (e.key === 'ArrowRight') next = (idx + 1) % tabs.length;
    else if (e.key === 'ArrowLeft') next = (idx - 1 + tabs.length) % tabs.length;
    else if (e.key === 'Home') next = 0;
    else if (e.key === 'End') next = tabs.length - 1;
    else return;
    e.preventDefault();
    onChange(tabs[next].key);
  };
  return (
    <div
      role="tablist"
      aria-label="Filter upstream by kind"
      className="flex items-center gap-px rounded border bg-muted/40 p-px text-[10px] uppercase tracking-wider"
    >
      {tabs.map((tab) => {
        const active = value === tab.key;
        return (
          <button
            key={tab.key}
            type="button"
            role="tab"
            aria-selected={active}
            tabIndex={active ? 0 : -1}
            onKeyDown={onKeyDown}
            onClick={() => onChange(tab.key)}
            className={`rounded-sm px-1.5 py-0.5 font-mono transition-colors ${
              active
                ? 'bg-background text-foreground'
                : 'text-muted-foreground hover:text-foreground'
            }`}
          >
            {tab.label}
            <span className="ml-1 tabular-nums opacity-60">{counts[tab.key]}</span>
          </button>
        );
      })}
    </div>
  );
}

// ----------------------------------------------------------------------------
// Live log feed
// ----------------------------------------------------------------------------

// Memoized row — WS pushes a fresh `rows` array every 4s but most rows
// are unchanged. Memoizing by row object identity skips the bulk of the
// re-render work. `i` (used only for fade opacity) is a prop so it too
// participates in memo equality.
const LiveLogRowItem = memo(function LiveLogRowItem({
  r,
  i,
  cols,
}: {
  r: LiveLogRow;
  i: number;
  cols: string;
}) {
  return (
    <li
      className={`grid gap-3 border-b px-4 py-2 last:border-b-0 hover:bg-muted/30 lg:items-center ${cols}`}
      style={{ opacity: 1 - i * 0.022 }}
    >
      <div className="hidden text-muted-foreground lg:block">
        {fmtTime(new Date(r.created_at + 'Z'))}
      </div>
      <div className="hidden lg:block">
        <span
          className={`rounded border px-1 py-0.5 text-[9px] font-medium uppercase ${kindBadgeClass(r.kind)}`}
        >
          {r.kind}
        </span>
      </div>
      <div className="truncate">{shortId(r.user_id || null, 12)}</div>
      <div className="hidden truncate lg:block">{r.subject || '—'}</div>
      <div className="hidden text-right tabular-nums lg:block">
        {r.kind === 'api' ? r.tokens.toLocaleString() : '—'}
      </div>
      <div className="hidden text-right tabular-nums text-muted-foreground lg:block">
        {r.latency_ms || '—'}
      </div>
      <div className="truncate text-[10px] text-muted-foreground lg:hidden">
        <span className={`mr-1 rounded border px-1 text-[9px] uppercase ${kindBadgeClass(r.kind)}`}>
          {r.kind}
        </span>
        {r.subject}
      </div>
      <div className="text-right">
        <span
          className={`rounded px-1.5 py-0.5 text-[10px] font-medium ${statusBadgeClass(r.kind, r.status)}`}
        >
          {r.status || '—'}
        </span>
      </div>
    </li>
  );
});

function LiveLogPanel({ rows }: { rows: LiveLogRow[] | null }) {
  const { t } = useTranslation();
  // Mirror what the row layout will be so headers and rows align perfectly.
  const cols =
    'grid-cols-[1fr_auto_44px] lg:grid-cols-[64px_44px_1fr_1fr_56px_52px_52px]';
  return (
    // `min-h-0` lets this card shrink inside the flex parent so the row
    // list scrolls internally instead of pushing the page.
    <Card className="flex h-full min-h-0 flex-col gap-0 py-0">
      <div
        className={`hidden shrink-0 gap-3 border-b px-4 py-2 text-[10px] uppercase tracking-wider text-muted-foreground lg:grid ${cols}`}
      >
        <div>{t('dashboard.time')}</div>
        <div>kind</div>
        <div>{t('dashboard.user')}</div>
        <div>{t('dashboard.subjectCol')}</div>
        <div className="text-right">{t('dashboard.tokens')}</div>
        <div className="text-right">ms</div>
        <div className="text-right">{t('dashboard.statusCol')}</div>
      </div>

      {rows === null ? (
        <div className="px-4 py-6 text-center font-mono text-xs text-muted-foreground">
          {t('common.loading')}
        </div>
      ) : rows.length === 0 ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-1 px-4 text-muted-foreground">
          <div className="font-mono text-xs">{t('dashboard.noTraffic')}</div>
          <div className="text-[10px] uppercase tracking-wider">{t('dashboard.noTrafficHint')}</div>
        </div>
      ) : (
        <ul className="min-h-0 flex-1 overflow-y-auto font-mono text-xs">
          {rows.map((r, i) => (
            <LiveLogRowItem key={r.id} r={r} i={i} cols={cols} />
          ))}
        </ul>
      )}
    </Card>
  );
}

// ----------------------------------------------------------------------------
// Provider health
// ----------------------------------------------------------------------------

// Fixed-height upstream-health panel with a scrollable list. Each row is a
// single line so 10+ providers fit comfortably without making the panel
// taller than the chart next to it.
const ProviderRow = memo(function ProviderRow({
  row,
  healthyLabel,
  degradedLabel,
  downLabel,
}: {
  row: ProviderHealth;
  healthyLabel: string;
  degradedLabel: string;
  downLabel: string;
}) {
  const cbReal = row.cb_state || '';
  const inferred: 'Closed' | 'HalfOpen' | 'Open' =
    row.success_rate >= 99 ? 'Closed' : row.success_rate >= 90 ? 'HalfOpen' : 'Open';
  const cb = (cbReal || inferred) as 'Closed' | 'HalfOpen' | 'Open';
  const status: 'healthy' | 'degraded' | 'down' =
    cb === 'Closed' ? 'healthy' : cb === 'HalfOpen' ? 'degraded' : 'down';
  const statusLabel =
    status === 'healthy' ? healthyLabel : status === 'degraded' ? degradedLabel : downLabel;
  const latency = Math.round(row.avg_latency_ms);
  return (
    <li className="flex items-center gap-2.5 px-3 py-2 text-xs hover:bg-muted/30">
      <ServiceLogo service={row.provider} className="shrink-0" />
      <div className="flex min-w-0 flex-1 flex-col">
        <span className="truncate font-mono">{row.provider}</span>
        <span className="truncate text-[10px] uppercase tracking-wide text-muted-foreground">
          {row.kind} · {row.requests.toLocaleString()} req · {latency}ms
        </span>
      </div>
      <span className="shrink-0 font-mono tabular-nums text-muted-foreground">
        {row.success_rate.toFixed(0)}%
      </span>
      <StatusIndicator status={status} label={statusLabel} pulse />
    </li>
  );
});

function ProviderHealthPanel({ rows }: { rows: ProviderHealth[] | null }) {
  const { t } = useTranslation();
  // Resolve labels once per render; ProviderRow is memoized by prop
  // identity so passing strings (not function references) keeps the row
  // stable across renders where only an adjacent row's metrics changed.
  const healthyLabel = t('common.healthy');
  const degradedLabel = t('dashboard.degraded');
  const downLabel = t('dashboard.down');
  return (
    <Card className="flex h-full min-h-0 flex-col gap-0 py-0">
      {rows === null ? (
        <div className="px-5 py-4 text-center text-[11px] text-muted-foreground">
          {t('common.loading')}
        </div>
      ) : rows.length === 0 ? (
        <div className="flex flex-1 items-center justify-center px-5 text-muted-foreground/40">
          <Inbox className="h-10 w-10" strokeWidth={1.25} />
        </div>
      ) : (
        <ul className="min-h-0 flex-1 divide-y overflow-y-auto">
          {rows.map((p) => (
            <ProviderRow
              key={`${p.kind}-${p.provider}`}
              row={p}
              healthyLabel={healthyLabel}
              degradedLabel={degradedLabel}
              downLabel={downLabel}
            />
          ))}
        </ul>
      )}
    </Card>
  );
}

// ----------------------------------------------------------------------------
// Sliding-window RPM area chart (shadcn AreaChart)
// ----------------------------------------------------------------------------

const rpmConfig = {
  count: { label: 'Requests', color: 'var(--chart-1)' },
} satisfies ChartConfig;

function RpmWindowPanel({
  buckets,
  maxRpm,
}: {
  buckets: number[] | null;
  maxRpm: number | null;
}) {
  const { t } = useTranslation();
  const data = buckets ?? Array(30).fill(0);
  const total = data.reduce((a, b) => a + b, 0);
  const avg = Math.round(total / Math.max(1, data.length));
  const last = data[data.length - 1] ?? 0;
  const peak = Math.max(...data, 0);

  const chartData = useMemo(
    () =>
      data.map((count, i) => ({
        minute: `-${data.length - 1 - i}m`,
        count,
      })),
    [data],
  );

  return (
    // Compact: header on one line (current rpm + avg/peak/total inline),
    // small chart, no footer. Total height ~140px.
    <Card size="sm" className="gap-2">
      <CardHeader className="flex-row items-baseline justify-between gap-3 pb-0">
        <div className="flex items-baseline gap-2 min-w-0">
          <span className="font-mono text-xl tabular-nums leading-none">
            {last.toLocaleString()}
          </span>
          <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
            /min
          </span>
        </div>
        <div className="flex items-baseline gap-3 text-[10px] text-muted-foreground">
          <span>
            <span className="uppercase tracking-wider">{t('dashboard.avgPerMin')} </span>
            <span className="font-mono tabular-nums text-foreground">{avg}</span>
          </span>
          <span>
            <span className="uppercase tracking-wider">{t('dashboard.peak')} </span>
            <span className="font-mono tabular-nums text-foreground">{peak}</span>
          </span>
          {maxRpm != null && (
            <span>
              <span className="uppercase tracking-wider">{t('dashboard.limit')} </span>
              <span className="font-mono tabular-nums text-foreground">{maxRpm}</span>
            </span>
          )}
        </div>
      </CardHeader>
      <CardContent className="px-2 pb-2">
        <ChartContainer config={rpmConfig} className="aspect-auto h-20 w-full">
          <AreaChart data={chartData} margin={{ left: 4, right: 4, top: 4, bottom: 0 }}>
            <defs>
              <linearGradient id="rpm-fill" x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="var(--color-count)" stopOpacity={0.45} />
                <stop offset="95%" stopColor="var(--color-count)" stopOpacity={0.05} />
              </linearGradient>
              <filter id="rpm-glow" x="-20%" y="-20%" width="140%" height="140%">
                <feGaussianBlur stdDeviation="2" result="blur" />
                <feMerge>
                  <feMergeNode in="blur" />
                  <feMergeNode in="SourceGraphic" />
                </feMerge>
              </filter>
            </defs>
            <CartesianGrid vertical={false} stroke="var(--border)" strokeOpacity={0.4} />
            <YAxis hide domain={[0, 'dataMax']} />
            <ChartTooltip cursor={false} content={<ChartTooltipContent indicator="line" />} />
            <Area
              dataKey="count"
              type="natural"
              stroke="var(--color-count)"
              strokeWidth={2}
              fill="url(#rpm-fill)"
              filter="url(#rpm-glow)"
              isAnimationActive={false}
            />
            {maxRpm != null && (
              <ReferenceLine
                y={maxRpm}
                stroke="var(--destructive)"
                strokeDasharray="4 4"
                strokeWidth={1}
              />
            )}
          </AreaChart>
        </ChartContainer>
      </CardContent>
    </Card>
  );
}
