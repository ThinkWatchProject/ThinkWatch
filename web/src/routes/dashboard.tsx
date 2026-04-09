import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from 'react';
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
import { api } from '@/lib/api';
import { DashboardLiveSchema, WsTicketSchema } from '@/lib/schemas';

interface DashboardStats {
  total_requests_today: number;
  active_providers: number;
  active_api_keys: number;
  connected_mcp_servers: number;
}

interface UsageStats {
  total_tokens_today: number;
  total_requests_today: number;
}

interface CostStats {
  total_cost_mtd: number;
  budget_usage_pct: number | null;
}

interface ProviderHealth {
  kind: 'ai' | 'mcp';
  provider: string;
  requests: number;
  avg_latency_ms: number;
  success_rate: number;
  cb_state: string; // "Closed" | "HalfOpen" | "Open" | ""
}

interface LiveLogRow {
  kind: 'api' | 'mcp';
  id: string;
  user_id: string;
  /** model_id for "api", tool_name for "mcp" */
  subject: string;
  /** numeric HTTP status as a string for "api", or status string for "mcp" */
  status: string;
  latency_ms: number;
  /** total tokens for "api", 0 for "mcp" */
  tokens: number;
  created_at: string;
}

interface DashboardLive {
  providers: ProviderHealth[];
  rpm_buckets: number[];
  recent_logs: LiveLogRow[];
  max_rpm_limit: number | null;
}

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

/** Append-only ring buffer of recent samples for sparklines. */
function useSparkHistory(value: number | undefined, length = 24) {
  const [hist, setHist] = useState<number[] | null>(null);
  useEffect(() => {
    if (value == null) return;
    setHist((prev) => {
      // Seed the buffer with the first value across the whole window so
      // the line starts flat instead of "rising from zero".
      if (prev === null) return Array(length).fill(value);
      return [...prev.slice(1), value];
    });
  }, [value, length]);
  return hist ?? Array(length).fill(0);
}

// ----------------------------------------------------------------------------
// Live snapshot via WebSocket. Falls back to a one-shot HTTP fetch if WS
// can't connect (e.g. behind a proxy that doesn't speak the upgrade).
// ----------------------------------------------------------------------------

function useLiveDashboard() {
  const [live, setLive] = useState<DashboardLive | null>(null);
  const [connected, setConnected] = useState(false);
  // Ref mirror so the WS callbacks can read "have we ever received data?"
  // without capturing a stale closure (the previous code captured `live`
  // at effect-mount time and the fallback fetch in onclose never fired
  // after the very first connect).
  const liveRef = useRef<DashboardLive | null>(null);
  liveRef.current = live;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;
    let backoff = 1000;

    const connect = async () => {
      if (cancelled) return;
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
          // Surface parse failures so they're at least visible in
          // devtools — the previous silent catch made WS bugs invisible.
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
            .catch(() => {});
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
        if (ws && ws.readyState === WebSocket.OPEN) ws.close();
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
      if (ws) {
        ws.onclose = null;
        ws.close();
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { live, connected };
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
}

function StatCard({ label, value, format, delta, spark, loading, chartIndex }: StatCardProps) {
  const animated = useCounter(value);
  const data = useMemo(() => spark.map((v, i) => ({ i, v })), [spark]);
  const config = {
    v: { label, color: `var(--chart-${chartIndex})` },
  } satisfies ChartConfig;

  return (
    <Card size="sm">
      <CardHeader>
        <CardDescription>{label}</CardDescription>
        <CardTitle className="font-mono text-2xl tabular-nums tracking-tight">
          {loading ? '…' : format(animated)}
        </CardTitle>
        {delta && (
          <CardAction>
            <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] font-medium text-muted-foreground">
              {delta}
            </span>
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
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [usage, setUsage] = useState<UsageStats | null>(null);
  const [cost, setCost] = useState<CostStats | null>(null);
  const { live, connected } = useLiveDashboard();

  useEffect(() => {
    api<DashboardStats>('/api/dashboard/stats').then(setStats).catch(() => {});
    api<UsageStats>('/api/analytics/usage/stats').then(setUsage).catch(() => {});
    api<CostStats>('/api/analytics/costs/stats').then(setCost).catch(() => {});
  }, []);

  const tokensSpark = useSparkHistory(usage?.total_tokens_today);
  const costSpark = useSparkHistory(cost?.total_cost_mtd);
  const keysSpark = useSparkHistory(stats?.active_api_keys);
  const rpmSpark = useMemo(() => live?.rpm_buckets ?? Array(24).fill(0), [live]);

  const currentRpm = live?.rpm_buckets?.[live.rpm_buckets.length - 1] ?? 0;
  const loading = !stats || !usage || !cost;

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

      <Section eyebrow={t('dashboard.overviewEyebrow')} className="shrink-0">
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <StatCard
            label={t('dashboard.tokensUsedToday')}
            value={usage?.total_tokens_today ?? 0}
            format={(v) => fmtCompact(v, locale)}
            spark={tokensSpark}
            loading={loading}
            chartIndex={1}
          />
          <StatCard
            label={t('dashboard.costMtd')}
            value={cost?.total_cost_mtd ?? 0}
            format={(v) => fmtUsd(v, locale)}
            delta={
              cost?.budget_usage_pct != null ? `${cost.budget_usage_pct.toFixed(1)}%` : undefined
            }
            spark={costSpark}
            loading={loading}
            chartIndex={2}
          />
          <StatCard
            label={t('dashboard.activeApiKeys')}
            value={stats?.active_api_keys ?? 0}
            format={(v) => fmtInt(Math.round(v), locale)}
            spark={keysSpark}
            loading={loading}
            chartIndex={3}
          />
          <StatCard
            label={t('dashboard.requestsPerMin')}
            value={currentRpm}
            format={(v) => fmtInt(Math.round(v), locale)}
            delta={t('dashboard.live')}
            spark={rpmSpark}
            loading={loading}
            chartIndex={4}
          />
        </div>
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
            <li
              key={r.id}
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
                <span
                  className={`mr-1 rounded border px-1 text-[9px] uppercase ${kindBadgeClass(r.kind)}`}
                >
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
function ProviderHealthPanel({ rows }: { rows: ProviderHealth[] | null }) {
  const { t } = useTranslation();
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
          {rows.map((p) => {
            const cbReal = p.cb_state || '';
            const inferred: 'Closed' | 'HalfOpen' | 'Open' =
              p.success_rate >= 99 ? 'Closed' : p.success_rate >= 90 ? 'HalfOpen' : 'Open';
            const cb = (cbReal || inferred) as 'Closed' | 'HalfOpen' | 'Open';
            const dotClass =
              cb === 'Closed'
                ? 'bg-foreground'
                : cb === 'HalfOpen'
                  ? 'bg-muted-foreground'
                  : 'bg-destructive';
            const cbBorder =
              cb === 'Open'
                ? 'border-destructive/40 text-destructive'
                : cb === 'HalfOpen'
                  ? 'border-muted-foreground/40 text-muted-foreground'
                  : 'border-border text-muted-foreground';
            const latency = Math.round(p.avg_latency_ms);
            // Accessible status label for the colored dot — colorblind users
            // and screen readers need a non-color text alternative.
            const statusLabel =
              cb === 'Closed' ? t('common.healthy') : cb === 'HalfOpen' ? t('dashboard.degraded') : t('dashboard.down');
            return (
              <li
                key={`${p.kind}-${p.provider}`}
                className="flex items-center gap-2 px-3 py-2 text-xs hover:bg-muted/30"
              >
                {/* status dot — color encodes the CB state, so we can drop
                    the textual "Healthy/Degraded/Down" label entirely */}
                <span
                  role="img"
                  aria-label={statusLabel}
                  title={statusLabel}
                  className={`h-1.5 w-1.5 shrink-0 rounded-full ${dotClass}`}
                />
                <span
                  className={`shrink-0 rounded border px-1 py-px font-mono text-[9px] uppercase ${kindBadgeClass(p.kind)}`}
                >
                  {p.kind}
                </span>
                <span className="min-w-0 flex-1 truncate font-mono">{p.provider}</span>
                <span className="font-mono tabular-nums text-muted-foreground">
                  {p.requests.toLocaleString()}r
                </span>
                <span className="font-mono tabular-nums">{latency}ms</span>
                <span className="w-11 text-right font-mono tabular-nums text-muted-foreground">
                  {p.success_rate.toFixed(0)}%
                </span>
                <span className={`shrink-0 rounded border px-1 font-mono text-[9px] ${cbBorder}`}>
                  {cb}
                </span>
              </li>
            );
          })}
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
            </defs>
            <CartesianGrid vertical={false} />
            <YAxis hide domain={[0, 'dataMax']} />
            <ChartTooltip cursor={false} content={<ChartTooltipContent indicator="line" />} />
            <Area
              dataKey="count"
              type="natural"
              stroke="var(--color-count)"
              strokeWidth={1.6}
              fill="url(#rpm-fill)"
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
