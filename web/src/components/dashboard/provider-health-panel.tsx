/**
 * Provider-health panel: one row per upstream (AI provider or MCP
 * server) showing request count, average latency, success / throttled
 * rates, and real circuit-breaker state. Eyebrow tab switcher filters
 * the list to all / AI-only / MCP-only.
 */

import { memo, useCallback, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Inbox } from 'lucide-react';

import { Card } from '@/components/ui/card';
import { ServiceLogo } from '@/components/ui/service-logo';
import { StatusIndicator } from '@/components/ui/status-indicator';
import type { ProviderHealth } from '@/lib/schemas';

import type { ProviderFilter } from './types';

/**
 * Tab switcher used in the upstream-health eyebrow. Three options:
 * all / ai / mcp. Designed to be visually quiet — uppercase tracking-
 * wider labels matching the eyebrow itself.
 */
export function ProviderFilterTabs({
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
    let next: number;
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

// Fixed-height upstream-health panel with a scrollable list. Each row is a
// single line so 10+ providers fit comfortably without making the panel
// taller than the chart next to it.
function ProviderRowImpl({
  row,
  healthyLabel,
  degradedLabel,
  downLabel,
  noTrafficTooltip,
  successRateTooltipFormat,
}: {
  row: ProviderHealth;
  healthyLabel: string;
  degradedLabel: string;
  downLabel: string;
  noTrafficTooltip: string;
  successRateTooltipFormat: (rate: number) => string;
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
      <span
        className="shrink-0 font-mono tabular-nums text-muted-foreground"
        title={
          row.success_rate === null
            ? noTrafficTooltip
            : successRateTooltipFormat(row.success_rate)
        }
      >
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
  if (prev.noTrafficTooltip !== next.noTrafficTooltip) return false;
  // Identity check on the formatter is sufficient — the parent
  // memoizes it across renders so a stable reference means stable
  // output for a given input.
  if (prev.successRateTooltipFormat !== next.successRateTooltipFormat) return false;
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

export function ProviderHealthPanel({ rows }: { rows: ProviderHealth[] | null }) {
  const { t } = useTranslation();
  // Resolve labels once per render; ProviderRow is memoized by prop
  // identity so passing strings (not function references) keeps the row
  // stable across renders where only an adjacent row's metrics changed.
  const healthyLabel = t('common.healthy');
  const degradedLabel = t('dashboard.degraded');
  const downLabel = t('dashboard.down');
  const noTrafficTooltip = t(
    'dashboard.noTrafficInWindow',
    'No traffic in window — no health signal',
  );
  const successRateTooltipFormat = useCallback(
    (rate: number) =>
      t('dashboard.successRateTooltip', '{{rate}}% successful (excludes 429 throttling)', {
        rate: rate.toFixed(0),
      }),
    [t],
  );
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
              noTrafficTooltip={noTrafficTooltip}
              successRateTooltipFormat={successRateTooltipFormat}
            />
          ))}
        </ul>
      )}
    </Card>
  );
}
