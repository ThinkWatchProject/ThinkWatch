/**
 * Active-users leaderboard — top N callers over the dashboard's
 * range (24h / 7d / 30d). Scrollable vertical list, ranked by
 * `request_count + mcp_call_count`. Wired into the live WS snapshot
 * so it refreshes on the same 4s cadence as the other panels.
 */

import { memo } from 'react';
import { useTranslation } from 'react-i18next';
import { Inbox } from 'lucide-react';

import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import type { TopActiveUser, TopActiveUsersResponse } from '@/lib/schemas';

import { fmtCompact } from './format';

/**
 * Small "N 人" badge for the active-users eyebrow. Reads from the
 * live snapshot's `top_users` envelope — no separate fetch — so the
 * badge ticks at the same cadence as the panel body. Renders nothing
 * while the live socket is still warming up or when the window is
 * empty.
 */
export function TopUsersTotalBadge({
  data,
  locale,
}: {
  data: TopActiveUsersResponse | null;
  locale: string;
}) {
  const { t } = useTranslation();
  if (data === null || data.total === 0) return null;
  return (
    <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
      {t('dashboard.totalUsers', {
        // No `count` here — neither en.json nor zh.json defines
        // _one / _other plural variants for this key, so passing it
        // would have no effect. If we want "1 user / 5 users" later,
        // add the plural variants and reintroduce the count arg.
        countStr: data.total.toLocaleString(locale),
      })}
    </span>
  );
}

export function TopUsersPanel({
  data,
  locale,
}: {
  data: TopActiveUsersResponse | null;
  locale: string;
}) {
  const { t } = useTranslation();
  const users = data?.users ?? null;

  // Layout mirrors LiveLogPanel: column header sits flush at the top
  // of the Card (sibling of the scroll list, not inside a CardContent
  // wrapper) so the panel's top edge IS the header's top edge — no
  // visible Card frame floating above the labels. `border-b` on the
  // header doubles as the divider between labels and rows.
  return (
    <Card className="flex h-full min-h-0 flex-col gap-0 py-0">
      <div className="flex shrink-0 items-center gap-2.5 border-b px-3 py-2 text-[10px] uppercase tracking-wider text-muted-foreground">
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
      {users === null ? (
        <div className="flex flex-col gap-2 px-3 py-3">
          {Array.from({ length: 5 }).map((_, i) => (
            <div key={i} className="flex items-center gap-2">
              <Skeleton className="h-4 w-6" />
              <Skeleton className="h-4 flex-1" />
              <Skeleton className="h-4 w-12" />
            </div>
          ))}
        </div>
      ) : users.length === 0 ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-3 text-center text-muted-foreground">
          <Inbox className="h-8 w-8" strokeWidth={1.25} />
          <span className="text-xs">{t('dashboard.noActiveUsers')}</span>
        </div>
      ) : (
        <ul
          className="min-h-0 flex-1 divide-y divide-border/40 overflow-y-auto"
          aria-label={t('dashboard.activeUsersEyebrow')}
        >
          {users.map((u, i) => (
            <TopUserRow key={u.user_id} rank={i + 1} user={u} locale={locale} />
          ))}
        </ul>
      )}
    </Card>
  );
}

// `memo` with a CUSTOM comparator because the `user` prop is a fresh
// object each WS tick — `live.top_users` comes from `JSON.parse` on
// every inbound frame, so default shallow compare (`Object.is`) always
// returns false on the object identity even when contents are equal,
// and the wrapper does nothing. The WS tick (4 s) is more frequent
// than the server-side top-users cache TTL (15 s), so 3 of every 4
// ticks produce identical rows — compare load-bearing scalars to
// skip the rebuild when nothing observable changed.
const TopUserRow = memo(
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
  },
  (prev, next) =>
    prev.rank === next.rank &&
    prev.locale === next.locale &&
    prev.user.user_id === next.user.user_id &&
    prev.user.user_email === next.user.user_email &&
    prev.user.request_count === next.user.request_count &&
    prev.user.total_tokens === next.user.total_tokens &&
    prev.user.mcp_call_count === next.user.mcp_call_count,
);

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
