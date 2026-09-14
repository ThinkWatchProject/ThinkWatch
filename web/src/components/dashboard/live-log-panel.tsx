/**
 * Live log feed — unified gateway + MCP rows pushed over WS every 4s.
 *
 * Each row collapses N raw events into a single `(kind, user_id,
 * subject)` group with a count chip; the panel auto-scrolls newest-
 * first, and an eyebrow pause toggle ([`LiveLogPauseButton`]) freezes
 * the visible list without breaking the upstream stream.
 */

import { memo, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useResetOnChange } from '@/hooks/use-reset-on-change';
import { Pause, Play } from 'lucide-react';

import { Card } from '@/components/ui/card';
import type { LiveLogRow } from '@/lib/schemas';

import { fmtTime, kindBadgeClass, shortId, statusBadgeClass } from './format';

/**
 * Tokens cell with a brief flash when the value grows. The
 * `flashId` prop is a counter the parent bumps on each grow event;
 * re-keying the inner span on that counter forces the CSS
 * `animation` to re-fire (CSS only triggers an animation when it's
 * first applied — re-applying the same class on the same element
 * is a no-op without a key change).
 */
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

/**
 * Pause/resume toggle for the live-log eyebrow `action` slot. Lifting
 * the button up there mirrors how `ProviderFilterTabs` lives on the
 * provider-health eyebrow — keeps the panel card free of a redundant
 * header row, and operators get a consistent "controls live in the
 * eyebrow" mental model.
 */
export function LiveLogPauseButton({
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

export function LiveLogPanel({
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
  // Snapshot on the pause edge only — keyed on `paused`, so a new batch of
  // rows arriving while paused does not overwrite what the user froze.
  useResetOnChange(paused, () => {
    setSnapshot(paused ? rows : null);
  });
  // If the live stream is reset mid-pause (range change clears `live`),
  // drop the snapshot too so the panel doesn't keep showing old-window
  // rows under the new range's eyebrow. Re-pause on the next WS frame
  // re-captures from the new window.
  useResetOnChange(rows, () => {
    if (paused && rows === null) setSnapshot(null);
  });
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
