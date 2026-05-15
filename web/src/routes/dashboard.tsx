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
import { GettingStartedCard } from '@/components/dashboard/getting-started-card';
import { useTranslation } from 'react-i18next';
import { Inbox, Pause, Play } from 'lucide-react';
import { Area, AreaChart } from 'recharts';
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { ChartContainer, type ChartConfig } from '@/components/ui/chart';
import { api, apiPut } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { StatusIndicator } from '@/components/ui/status-indicator';
import { ServiceLogo } from '@/components/ui/service-logo';
import {
  DashboardLiveSchema,
  TopActiveUsersResponseSchema,
  WsTicketSchema,
  type DashboardLive,
  type LiveLogRow,
  type ProviderHealth,
  type TopActiveUser,
  type TopActiveUsersResponse,
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
  // Decimal string on the wire (CH `Decimal(18, 10)`). Dashboard
  // converts to Number for the card widgets — the backend is the
  // source of truth for billing-grade precision; the dashboard just
  // needs the rough value for display.
  total_cost: string;
  budget_usage_pct: number | null;
  cost_buckets: string[];
  range: string;
  total_cost_mtd: string;
  prev_total_cost?: string;
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
  const { t } = useTranslation();
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

// 12s is past every realistic CH analytics query (P99 < 3s on the
// existing dashboards) but short enough that a frozen result lands
// the per-card error fallback well before a user gives up scrolling.
const DASHBOARD_CARD_TIMEOUT_MS = 12_000;

/// Race a promise against a deadline; on timeout reject with a
/// labelled Error that the ErrorBoundary surfaces. The cleared timer
/// keeps the JS heap clean when the underlying request resolves first.
function withTimeout<T>(p: Promise<T>, ms: number, label: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const id = setTimeout(() => {
      reject(new Error(`Timed out fetching ${label} (>${ms}ms)`));
    }, ms);
    p.then(
      (v) => {
        clearTimeout(id);
        resolve(v);
      },
      (e) => {
        clearTimeout(id);
        reject(e);
      },
    );
  });
}

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
  // Race each fetch against a generous timeout so a stuck endpoint
  // surfaces as the per-card error fallback instead of an indefinite
  // skeleton — Suspense alone has no timeout, and the user can't tell
  // "loading slowly" from "the page is broken" without one.
  const compareQs = compare ? '&compare=true' : '';
  const statsPromise = useMemo(
    () =>
      withTimeout(
        api<DashboardStats>(`/api/dashboard/stats?range=${range}${compareQs}`),
        DASHBOARD_CARD_TIMEOUT_MS,
        'dashboard.stats',
      ),
    [range, compareQs],
  );
  const usagePromise = useMemo(
    () =>
      withTimeout(
        api<UsageStats>(`/api/analytics/usage/stats?range=${range}${compareQs}`),
        DASHBOARD_CARD_TIMEOUT_MS,
        'analytics.usage.stats',
      ),
    [range, compareQs],
  );
  const costPromise = useMemo(
    () =>
      withTimeout(
        api<CostStats>(`/api/analytics/costs/stats?range=${range}${compareQs}`),
        DASHBOARD_CARD_TIMEOUT_MS,
        'analytics.costs.stats',
      ),
    [range, compareQs],
  );
  // Range-keyed top-users leaderboard. Compare mode doesn't apply —
  // the panel shows current-window rankings only, not deltas — so
  // the promise rebuilds only when `range` itself changes.
  const topUsersPromise = useMemo(
    () =>
      withTimeout(
        api<TopActiveUsersResponse>(`/api/dashboard/top-users?range=${range}`, {
          schema: TopActiveUsersResponseSchema,
        }),
        DASHBOARD_CARD_TIMEOUT_MS,
        'dashboard.top-users',
      ),
    [range],
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

  // Top-grid RPM tile uses the last bucket of `rpm_buckets` for
  // "current rate" and the whole array as a sparkline. The bottom-
  // right panel renders a DIFFERENT metric now (active users), so
  // the two are no longer redundant — top is "how fast", bottom is
  // "who's calling".
  const rpmSpark = useMemo(() => live?.rpm_buckets ?? Array(24).fill(0), [live]);
  const currentRpm = live?.rpm_buckets?.[live.rpm_buckets.length - 1] ?? 0;

  // Upstream-health filter (all / ai / mcp). Counts come from the live
  // snapshot so the tab pills always show the current per-kind totals.
  const [providerFilter, setProviderFilter] = useState<ProviderFilter>('all');
  // Live-log pause state lifted from the panel so the toggle can live
  // in the Section eyebrow alongside the title.
  const [livePaused, setLivePaused] = useState(false);
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

      <GettingStartedCard
        signals={{
          // Use the live snapshot so the card disappears the moment
          // the user finishes the action that satisfied a step,
          // without an extra round-trip.
          hasProviders: (live?.providers?.length ?? 0) > 0,
          // Treat any RPM activity as evidence that an API key is in
          // use. False until first request — fine for first-run.
          hasApiKeys: (live?.rpm_buckets ?? []).some((v) => v > 0),
        }}
      />

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
              // RPM streams over WS — no Suspense needed, but it
              // reads `live?.rpm_buckets` which stays null-safe
              // until the first frame arrives. Distinct from the
              // bottom-right active-users panel: this is request
              // rate, that one is concurrent users.
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
          action={
            <LiveLogPauseButton
              paused={livePaused}
              onToggle={() => setLivePaused((p) => !p)}
            />
          }
        >
          <LiveLogPanel rows={live?.recent_logs ?? null} paused={livePaused} />
        </Section>

        <div className="flex min-h-0 flex-col gap-4">
          {/* Upstream health and the active-users leaderboard split the
              right column 50/50. Both panels are list-shaped and
              scrollable internally, so equal flex-basis + min-h-0
              lets either grow to fill its half regardless of how many
              rows it contains. */}
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
          <Section
            eyebrow={t('dashboard.activeUsersEyebrow')}
            className="flex min-h-0 flex-1 flex-col"
            action={
              <ErrorBoundary fallback={null}>
                <Suspense fallback={null}>
                  <TopUsersTotalBadge promise={topUsersPromise} locale={locale} />
                </Suspense>
              </ErrorBoundary>
            }
          >
            <ErrorBoundary fallback={<TopUsersPanelError />}>
              <Suspense fallback={<TopUsersPanelSkeleton />}>
                <TopUsersPanel promise={topUsersPromise} locale={locale} />
              </Suspense>
            </ErrorBoundary>
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
  const { t } = useTranslation();
  const tabs: { key: ProviderFilter; label: string }[] = [
    { key: 'all', label: t('dashboard.providerFilter.all') },
    { key: 'ai', label: t('dashboard.providerFilter.ai') },
    { key: 'mcp', label: t('dashboard.providerFilter.mcp') },
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
/// Tokens cell with a brief flash when the value grows. The
/// `flashId` prop is a counter the parent bumps on each grow event;
/// re-keying the inner span on that counter forces the CSS
/// `animation` to re-fire (CSS only triggers an animation when it's
/// first applied — re-applying the same class on the same element
/// is a no-op without a key change).
function TokensCell({
  kind,
  tokens,
  flashId,
}: {
  kind: 'api' | 'mcp';
  tokens: number;
  flashId: number;
}) {
  if (kind !== 'api') {
    return <div className="hidden text-right tabular-nums lg:block">—</div>;
  }
  return (
    <div className="hidden text-right tabular-nums lg:block">
      <span
        key={flashId}
        className={flashId > 0 ? 'animate-token-flip' : 'inline-block'}
      >
        {tokens.toLocaleString()}
      </span>
    </div>
  );
}

const LiveLogRowItem = memo(function LiveLogRowItem({
  r,
  i,
  cols,
}: {
  r: LiveLogRow;
  i: number;
  cols: string;
}) {
  // Flash the tokens cell green for ~700ms whenever the value grows
  // — operators told us they wanted a feedback signal that a row was
  // *just* hit, since the aggregated feed otherwise looks frozen
  // between WS frames. We never flash on decrease (tokens is a sum
  // over a 15-min window; a drop would mean an older event aged out
  // and isn't an "activity" event).
  const prevTokens = useRef(r.tokens);
  const [flashId, setFlashId] = useState(0);
  useEffect(() => {
    if (r.tokens > prevTokens.current) {
      setFlashId((n) => n + 1);
    }
    prevTokens.current = r.tokens;
  }, [r.tokens]);
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
      <div className="hidden truncate lg:block">
        {r.subject || '—'}
        {r.count > 1 && (
          <span
            className="ml-1.5 rounded bg-muted px-1 py-0.5 font-mono text-[9px] tabular-nums text-muted-foreground"
            title={`${r.count} requests in the last 15 minutes`}
          >
            ×{r.count}
          </span>
        )}
      </div>
      <TokensCell kind={r.kind} tokens={r.tokens} flashId={flashId} />
      <div className="hidden text-right tabular-nums text-muted-foreground lg:block">
        {r.latency_ms || '—'}
      </div>
      <div className="truncate text-[10px] text-muted-foreground lg:hidden">
        <span className={`mr-1 rounded border px-1 text-[9px] uppercase ${kindBadgeClass(r.kind)}`}>
          {r.kind}
        </span>
        {r.subject}
        {r.count > 1 && <span className="ml-1 tabular-nums">×{r.count}</span>}
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

/// Pause/resume toggle for the live-log eyebrow `action` slot. Lifting
/// the button up there mirrors how `ProviderFilterTabs` lives on the
/// provider-health eyebrow — keeps the panel card free of a redundant
/// header row, and operators get a consistent "controls live in the
/// eyebrow" mental model.
function LiveLogPauseButton({
  paused,
  onToggle,
}: {
  paused: boolean;
  onToggle: () => void;
}) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onToggle}
      className={`inline-flex items-center gap-1 rounded border px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider transition-colors ${
        paused
          ? 'border-primary/60 bg-primary/10 text-primary'
          : 'border-border bg-muted/30 text-muted-foreground hover:text-foreground'
      }`}
      aria-pressed={paused}
      title={paused ? t('dashboard.resume') : t('dashboard.pause')}
    >
      {paused ? <Play className="h-3 w-3" /> : <Pause className="h-3 w-3" />}
      {paused ? t('dashboard.resume') : t('dashboard.pause')}
    </button>
  );
}

function LiveLogPanel({
  rows,
  paused,
}: {
  rows: LiveLogRow[] | null;
  // Pause toggle lives on the Section's eyebrow now (sibling control
  // pattern, like upstream-health's filter tabs). The panel still
  // owns the freeze logic — snapshot the rows when `paused` flips on,
  // forget the snapshot when it flips off — so live frames stop
  // scrolling the visible list out from under the operator.
  paused: boolean;
}) {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<LiveLogRow[] | null>(null);
  useEffect(() => {
    if (paused) {
      setSnapshot(rows);
    } else {
      setSnapshot(null);
    }
    // Intentionally ignore `rows` — we only snapshot on the pause edge.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [paused]);
  const displayed = paused ? snapshot : rows;

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
        <div>{t('dashboard.kindCol')}</div>
        <div>{t('dashboard.user')}</div>
        <div>{t('dashboard.subjectCol')}</div>
        <div className="text-right">{t('dashboard.tokens')}</div>
        <div className="text-right">{t('dashboard.unitMs')}</div>
        <div className="text-right">{t('dashboard.statusCol')}</div>
      </div>

      {displayed === null ? (
        <div className="px-4 py-6 text-center font-mono text-xs text-muted-foreground">
          {t('common.loading')}
        </div>
      ) : displayed.length === 0 ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-1 px-4 text-muted-foreground">
          <div className="font-mono text-xs">{t('dashboard.noTraffic')}</div>
          <div className="text-[10px] uppercase tracking-wider">{t('dashboard.noTrafficHint')}</div>
        </div>
      ) : (
        <ul className="min-h-0 flex-1 overflow-y-auto font-mono text-xs">
          {displayed.map((r, i) => (
            // Composite key by aggregation tuple, NOT r.id —
            // `argMax(id)` rotates each time a new event lands in
            // the group, which would force React to unmount/remount
            // the row on every tick and cancel any in-flight
            // animations. Stable identity = animation can detect
            // "this row's tokens just grew."
            <LiveLogRowItem
              key={`${r.kind}-${r.user_id}-${r.subject}`}
              r={r}
              i={i}
              cols={cols}
            />
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
function ProviderRowImpl({
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
  // success_rate may be null (no traffic in the window). When it is,
  // the inferred CB state defaults to Closed so a quiet-but-configured
  // upstream still reads as "healthy" — the real signal then comes
  // from the registry's cb_state if it's been opened by the gateway.
  const inferred: 'Closed' | 'HalfOpen' | 'Open' =
    row.success_rate === null || row.success_rate >= 99
      ? 'Closed'
      : row.success_rate >= 90
        ? 'HalfOpen'
        : 'Open';
  const cb = (cbReal || inferred) as 'Closed' | 'HalfOpen' | 'Open';
  const status: 'healthy' | 'degraded' | 'down' =
    cb === 'Closed' ? 'healthy' : cb === 'HalfOpen' ? 'degraded' : 'down';
  const statusLabel =
    status === 'healthy' ? healthyLabel : status === 'degraded' ? degradedLabel : downLabel;
  const latency = Math.round(row.avg_latency_ms);
  // Only badge when the throttled rate clears 5% — below that it's
  // probably one-off 429s buried in noise. Null (no traffic) skips
  // the badge entirely.
  const throttled = row.throttled_rate !== null && row.throttled_rate >= 5;
  return (
    <li className="flex items-center gap-2.5 px-3 py-2 text-xs hover:bg-muted/30">
      <ServiceLogo service={row.provider} className="shrink-0" />
      <div className="flex min-w-0 flex-1 flex-col">
        <span className="truncate font-mono">{row.provider}</span>
        <span className="truncate text-[10px] uppercase tracking-wide text-muted-foreground">
          {row.kind} · {row.requests.toLocaleString()} req · {latency}ms
        </span>
      </div>
      {throttled && row.throttled_rate !== null && (
        <span
          className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wide text-amber-600 dark:text-amber-400"
          title={`${row.throttled_rate.toFixed(0)}% throttled (429)`}
        >
          {row.throttled_rate.toFixed(0)}% 429
        </span>
      )}
      <span className="shrink-0 font-mono tabular-nums text-muted-foreground">
        {row.success_rate === null ? '—' : `${row.success_rate.toFixed(0)}%`}
      </span>
      <StatusIndicator status={status} label={statusLabel} pulse />
    </li>
  );
}

// Custom areEqual: each WS tick produces freshly-deserialised row
// objects, so reference equality always reports "changed" and every
// row re-renders on every tick. A field-by-field compare against the
// previous render's row sidesteps that — when only one provider's
// numbers actually moved we re-render exactly that one <li>.
const ProviderRow = memo(ProviderRowImpl, (prev, next) => {
  if (prev.healthyLabel !== next.healthyLabel) return false;
  if (prev.degradedLabel !== next.degradedLabel) return false;
  if (prev.downLabel !== next.downLabel) return false;
  const a = prev.row;
  const b = next.row;
  return (
    a.provider === b.provider &&
    a.kind === b.kind &&
    a.requests === b.requests &&
    a.success_rate === b.success_rate &&
    a.throttled_rate === b.throttled_rate &&
    a.avg_latency_ms === b.avg_latency_ms &&
    (a.cb_state || '') === (b.cb_state || '')
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
// Active-users leaderboard — top N callers over the dashboard's range
// (24h / 7d / 30d). Scrollable vertical list, ranked by request count.
// ----------------------------------------------------------------------------

/// Small "N 人" badge for the active-users eyebrow. Reuses the same
/// promise as the panel body — React 19's `use()` deduplicates the
/// fetch, so this isn't a second round-trip. Renders nothing while
/// the promise is pending (the panel skeleton already signals
/// loading state) and nothing on error (the body's error fallback
/// covers it).
function TopUsersTotalBadge({
  promise,
  locale,
}: {
  promise: Promise<TopActiveUsersResponse>;
  locale: string;
}) {
  const { t } = useTranslation();
  const data = use(promise);
  if (data.total === 0) return null;
  return (
    <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
      {t('dashboard.totalUsers', {
        count: data.total,
        countStr: data.total.toLocaleString(locale),
      })}
    </span>
  );
}

function TopUsersPanelSkeleton() {
  return (
    <Card size="sm" className="flex h-full min-h-0 flex-col gap-0 py-0">
      <CardContent className="flex flex-col gap-2 px-3 py-3">
        {Array.from({ length: 5 }).map((_, i) => (
          <div key={i} className="flex items-center gap-2">
            <Skeleton className="h-4 w-6" />
            <Skeleton className="h-4 flex-1" />
            <Skeleton className="h-4 w-12" />
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

function TopUsersPanelError() {
  const { t } = useTranslation();
  return (
    <Card size="sm" className="flex h-full min-h-0 flex-col gap-0 py-0">
      <CardContent className="flex flex-1 items-center justify-center px-3 text-center text-xs text-muted-foreground">
        {t('dashboard.loadFailedShort', 'Failed to load')}
      </CardContent>
    </Card>
  );
}

function TopUsersPanel({
  promise,
  locale,
}: {
  promise: Promise<TopActiveUsersResponse>;
  locale: string;
}) {
  const { t } = useTranslation();
  const data = use(promise);
  const users = data.users;

  if (users.length === 0) {
    return (
      <Card size="sm" className="flex h-full min-h-0 flex-col gap-0 py-0">
        <CardContent className="flex flex-1 flex-col items-center justify-center gap-2 px-3 text-center text-muted-foreground">
          <Inbox className="h-8 w-8" strokeWidth={1.25} />
          <span className="text-xs">{t('dashboard.noActiveUsers')}</span>
        </CardContent>
      </Card>
    );
  }

  // `h-full` (not `flex-1`) because the Section's inner wrapper is
  // sized but not a flex container — same trick ProviderHealthPanel
  // uses to fill its half of the right column. CardContent owns the
  // scroll so a long top-50 list never pushes the upstream-health
  // panel out of view.
  // The column-header strip sits inside the scroll area but with
  // `position: sticky; top: 0` so it floats at the top while the
  // list scrolls underneath. Labels appear once instead of repeating
  // on every row — readable at any list length.
  return (
    <Card size="sm" className="flex h-full min-h-0 flex-col gap-0 py-0">
      <CardContent className="min-h-0 flex-1 overflow-y-auto px-0 py-0">
        <div
          className="sticky top-0 z-10 flex h-5 items-center gap-2.5 border-b bg-card/95 px-3 text-[9px] uppercase leading-none tracking-wider text-muted-foreground backdrop-blur"
        >
          <span className="w-4 shrink-0" aria-hidden="true" />
          <span className="min-w-0 flex-1" aria-hidden="true" />
          <span className="w-12 shrink-0 text-right font-mono tabular-nums">
            {t('dashboard.statApi', 'API')}
          </span>
          <span className="w-12 shrink-0 text-right font-mono tabular-nums">
            {t('dashboard.statTokens', 'TOK')}
          </span>
          <span className="w-12 shrink-0 text-right font-mono tabular-nums">
            {t('dashboard.statMcp', 'MCP')}
          </span>
        </div>
        <ul className="divide-y divide-border/40">
          {users.map((u, i) => (
            <TopUserRow key={u.user_id} rank={i + 1} user={u} locale={locale} />
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

function TopUserRow({
  rank,
  user,
  locale,
}: {
  rank: number;
  user: TopActiveUser;
  locale: string;
}) {
  // Email present → primary label is email, secondary is short user_id.
  // Email blank (pre-email-column rows / anonymous) → fall back to the
  // user_id so the row never reads as "user with no name."
  const label = user.user_email || user.user_id;
  const subLabel = user.user_email ? user.user_id.slice(0, 8) : null;
  // Three right-aligned numbers — labels live in the sticky header
  // up top so the rows themselves stay scannable. Zero values dim so
  // operators can tell "MCP-only" callers from "API-only" at a glance
  // without reading every digit.
  return (
    <li className="flex items-center gap-2.5 px-3 py-1.5 text-xs">
      <span className="w-4 shrink-0 text-right font-mono tabular-nums text-[10px] text-muted-foreground">
        {rank}
      </span>
      <div className="flex min-w-0 flex-1 flex-col leading-tight">
        <span className="truncate font-mono">{label}</span>
        {subLabel && (
          <span className="truncate text-[10px] text-muted-foreground">{subLabel}</span>
        )}
      </div>
      <TopUserStat value={user.request_count} locale={locale} />
      <TopUserStat value={user.total_tokens} locale={locale} />
      <TopUserStat value={user.mcp_call_count} locale={locale} />
    </li>
  );
}

function TopUserStat({ value, locale }: { value: number; locale: string }) {
  const zero = value === 0;
  return (
    <span
      className={`w-12 shrink-0 text-right font-mono tabular-nums text-[11px] ${
        zero ? 'text-muted-foreground/50' : ''
      }`}
    >
      {fmtCompact(value, locale)}
    </span>
  );
}
