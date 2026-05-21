import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { GettingStartedCard } from '@/components/dashboard/getting-started-card';
import { LiveLogPanel, LiveLogPauseButton } from '@/components/dashboard/live-log-panel';
import {
  ProviderFilterTabs,
  ProviderHealthPanel,
} from '@/components/dashboard/provider-health-panel';
import { Section } from '@/components/dashboard/section';
import {
  CostCard,
  DASHBOARD_CARD_TIMEOUT_MS,
  KeysCard,
  StatCard,
  StatCardGrid,
  SuspendedCard,
  TokensCard,
  withTimeout,
} from '@/components/dashboard/stat-cards';
import { TopUsersPanel, TopUsersTotalBadge } from '@/components/dashboard/top-users-panel';
import { useLiveDashboard } from '@/components/dashboard/use-live-dashboard';
import {
  TIME_RANGES,
  type CostStats,
  type DashboardStats,
  type ProviderFilter,
  type TimeRange,
  type UsageStats,
} from '@/components/dashboard/types';
import { fmtInt } from '@/components/dashboard/format';
import { api } from '@/lib/api';

export function DashboardPage() {
  const { t, i18n } = useTranslation();
  // Map i18next language code → BCP 47 locale for Intl.NumberFormat.
  const locale = i18n.language === 'zh' ? 'zh-CN' : 'en-US';

  // Global time-range filter. Selecting a different range remounts the
  // three Suspense cards (see `key={range}` below), which mints fresh
  // promises for the new window. The live WS also reconnects with the
  // new range so its embedded top-users leaderboard tracks the same
  // window.
  const [range, setRange] = useState<TimeRange>(() => {
    const cached = typeof window !== 'undefined' ? window.localStorage.getItem('dashboard.range.v1') : null;
    return cached && (TIME_RANGES as readonly string[]).includes(cached) ? (cached as TimeRange) : '24h';
  });
  const { live, connected } = useLiveDashboard(range);
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
    // `allProviders` is derived from `live` on the same render, so
    // including both as deps would be redundant — `allProviders`
    // alone reflects the live-state change.
  }, [allProviders, providerFilter]);

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
            action={<TopUsersTotalBadge data={live?.top_users ?? null} locale={locale} />}
          >
            <TopUsersPanel data={live?.top_users ?? null} locale={locale} />
          </Section>
        </div>
      </div>
    </div>
  );
}
