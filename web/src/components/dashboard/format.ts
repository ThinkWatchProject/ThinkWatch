/**
 * Display formatters and status-badge class helpers shared across the
 * dashboard panels.
 *
 * The number formatters are locale-aware — they read the caller's
 * current i18next language at format time so a runtime language toggle
 * picks up the new locale without remounting the cards. We don't
 * memoize the `Intl.NumberFormat` instances aggressively because
 * construction is fast and the language only changes on user action.
 */

export function fmtCompact(v: number, locale: string): string {
  return new Intl.NumberFormat(locale, { notation: 'compact', maximumFractionDigits: 1 }).format(v);
}

export function fmtUsd(v: number, locale: string): string {
  return new Intl.NumberFormat(locale, {
    style: 'currency',
    currency: 'USD',
    maximumFractionDigits: 2,
  }).format(v);
}

export function fmtInt(v: number, locale: string): string {
  return new Intl.NumberFormat(locale).format(v);
}

/** HH:MM:SS using local time. */
export function fmtTime(d: Date): string {
  return [d.getHours(), d.getMinutes(), d.getSeconds()]
    .map((n) => String(n).padStart(2, '0'))
    .join(':');
}

export function shortId(s: string | null, n = 8): string {
  if (!s) return '—';
  return s.length > n ? s.slice(0, n) : s;
}

/**
 * Tailwind class for the small status pill in the live-log feed.
 * Distinct mapping for `api` (numeric HTTP codes) and `mcp` (string
 * statuses) — kept in one helper so the row component doesn't have
 * to branch on `r.kind` twice.
 */
export function statusBadgeClass(kind: 'api' | 'mcp', status: string): string {
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

/**
 * Kind chip on each live-log row.
 * "api" (log row) and "ai" (provider) are both AI gateway things → primary tint.
 * "mcp" → muted.
 */
export function kindBadgeClass(kind: string): string {
  return kind === 'mcp'
    ? 'border-border bg-muted text-foreground'
    : 'border-primary/30 bg-primary/10 text-primary';
}
