import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { subHours, format } from 'date-fns';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import { Select, SelectContent, SelectItem, SelectTrigger } from '@/components/ui/select';
import { Search, FileText, ChevronDown, ChevronRight, Plus, Minus } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';
import { Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious } from '@/components/ui/pagination';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type LogCategory = 'gateway' | 'mcp' | 'audit' | 'platform' | 'access' | 'app';

interface LogEntry {
  id: string;
  created_at: string;
  [key: string]: unknown;
}

interface LogsResponse {
  items: LogEntry[];
  total: number;
}

const CATEGORY_API: Record<LogCategory, string> = {
  gateway: '/api/gateway/logs',
  mcp: '/api/mcp/logs',
  audit: '/api/audit/logs',
  platform: '/api/admin/platform-logs',
  access: '/api/admin/access-logs',
  app: '/api/admin/app-logs',
};

const PAGE_SIZE = 50;

// ---------------------------------------------------------------------------
// Query syntax parser: "level:error target:auth some text" →
//   { level: "error", target: "auth", q: "some text" }
// ---------------------------------------------------------------------------

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

interface ParsedQuery {
  /** Positive filters: key=value or `q` for free text. */
  params: Record<string, string>;
  /** Negative filters from `-key:value` tokens, kept as raw `key:value` strings. */
  excludes: string[];
}

function parseQuery(input: string): ParsedQuery {
  const params: Record<string, string> = {};
  const excludes: string[] = [];
  const freeText: string[] = [];
  // Match optional leading "-" then key:value (value can be quoted)
  const regex = /(-?)(\w+):(?:"([^"]*)"|(\S+))/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = regex.exec(input)) !== null) {
    // Collect text before this match as free text
    const before = input.slice(lastIndex, match.index).trim();
    if (before) freeText.push(before);
    lastIndex = regex.lastIndex;
    const negate = match[1] === '-';
    const key = match[2];
    const value = match[3] ?? match[4];
    if (negate) {
      // Re-quote if value contains commas/colons/spaces so the backend
      // splitter sees it as one token.
      const needsQuotes = /[\s,:]/.test(value);
      excludes.push(`${key}:${needsQuotes ? `"${value}"` : value}`);
    } else {
      params[key] = value;
    }
  }
  const after = input.slice(lastIndex).trim();
  if (after) freeText.push(after);
  if (freeText.length > 0) params.q = freeText.join(' ');
  return { params, excludes };
}

// ---------------------------------------------------------------------------
// Column definitions per category
// ---------------------------------------------------------------------------

interface ColDef {
  key: string;
  label: string;
  align?: 'right';
  mono?: boolean;
  render?: (v: unknown, row: LogEntry) => React.ReactNode;
  /**
   * Backend search key to use when the user clicks "+" to filter by this
   * cell's value. If unset, the cell is not filterable. Distinct from `key`
   * because some columns (e.g. `model_id`) map to a shorter search key
   * (`model`).
   */
  filterKey?: string;
  /**
   * Field on the row to read for the filter value when it differs from
   * `key`. For example, the user column displays `user_email` but should
   * filter by `user_id`.
   */
  filterValueKey?: string;
}

function statusBadge(code: unknown) {
  const c = Number(code);
  if (!c) return <Badge variant="outline">—</Badge>;
  if (c >= 200 && c < 300) return <Badge variant="default">{c}</Badge>;
  if (c >= 400) return <Badge variant="destructive">{c}</Badge>;
  return <Badge variant="secondary">{c}</Badge>;
}

function levelBadge(level: unknown) {
  const l = String(level).toUpperCase();
  if (l === 'ERROR') return <Badge variant="destructive">{l}</Badge>;
  if (l === 'WARN') return <Badge className="bg-yellow-600 text-white">{l}</Badge>;
  if (l === 'DEBUG' || l === 'TRACE') return <Badge variant="secondary">{l}</Badge>;
  return <Badge variant="outline">{l}</Badge>;
}

function getColumns(cat: LogCategory): ColDef[] {
  switch (cat) {
    case 'gateway':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'model_id', label: 'Model', mono: true, filterKey: 'model' },
        { key: 'provider', label: 'Provider', filterKey: 'provider' },
        { key: 'input_tokens', label: 'In', align: 'right' },
        { key: 'output_tokens', label: 'Out', align: 'right' },
        { key: 'cost_usd', label: 'Cost', align: 'right', render: (v) => `$${parseFloat(String(v || 0)).toFixed(4)}` },
        { key: 'latency_ms', label: 'Latency', align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status_code', label: 'Status', render: (v) => statusBadge(v), filterKey: 'status_code' },
      ];
    case 'mcp':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'tool_name', label: 'Tool', mono: true, filterKey: 'tool_name' },
        { key: 'server_name', label: 'Server', filterKey: 'server_id', filterValueKey: 'server_id' },
        { key: 'duration_ms', label: 'Duration', align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status', label: 'Status', render: (v) => <Badge variant={v === 'success' ? 'default' : 'destructive'}>{String(v)}</Badge>, filterKey: 'status' },
        { key: 'user_email', label: 'User', filterKey: 'user_id', filterValueKey: 'user_id' },
      ];
    case 'audit':
      return [
        { key: 'timestamp', label: 'Time' },
        { key: 'user_email', label: 'User', filterKey: 'user_id', filterValueKey: 'user_id' },
        { key: 'action', label: 'Action', filterKey: 'action' },
        { key: 'resource', label: 'Resource', filterKey: 'resource' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'platform':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'user_email', label: 'User', filterKey: 'user_id', filterValueKey: 'user_id' },
        { key: 'action', label: 'Action', filterKey: 'action' },
        { key: 'resource', label: 'Resource', filterKey: 'resource' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'access':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'method', label: 'Method', filterKey: 'method' },
        { key: 'path', label: 'Path', mono: true, filterKey: 'path' },
        { key: 'status_code', label: 'Status', render: (v) => statusBadge(v), filterKey: 'status_code' },
        { key: 'latency_ms', label: 'Latency', align: 'right', render: (v) => `${v}ms` },
        { key: 'port', label: 'Port', filterKey: 'port' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'app':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'level', label: 'Level', render: (v) => levelBadge(v), filterKey: 'level' },
        { key: 'target', label: 'Target', mono: true, filterKey: 'target' },
        { key: 'message', label: 'Message' },
        { key: 'span', label: 'Span' },
      ];
  }
}

function getTimeKey(cat: LogCategory): string {
  return cat === 'audit' ? 'timestamp' : 'created_at';
}

// ---------------------------------------------------------------------------
// Local <-> UTC time conversion
//
// The DateTimeRangePicker emits "yyyy-MM-ddTHH:mm" strings in the browser's
// local time zone (no offset suffix). The backend stores everything as UTC.
// We need to convert between the two:
//   - localToUtcQuery: turn "2026-04-06T17:21" (local) into the
//     "2026-04-06 09:21:00" (UTC) string the backend expects
//   - utcQueryToLocal: reverse, used when reading the value back from the URL
// ---------------------------------------------------------------------------

// Local "yyyy-MM-ddTHH:mm" → UTC "yyyy-MM-dd HH:mm:ss" string for the backend.
function localToUtcQuery(local: string): string {
  if (!local) return '';
  const d = new Date(local);
  if (Number.isNaN(d.getTime())) return '';
  // d represents the local wall-clock time. Convert to UTC components.
  const pad = (n: number) => String(n).padStart(2, '0');
  return (
    `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}` +
    ` ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`
  );
}

function defaultFromLocal(): string {
  return format(subHours(new Date(), 1), "yyyy-MM-dd'T'HH:mm");
}

function defaultToLocal(): string {
  return format(new Date(), "yyyy-MM-dd'T'HH:mm");
}

// "Highlight" fields rendered above the raw JSON in the per-row expansion.
// Order matters; the first listed fields show first.
const DETAIL_HIGHLIGHTS: Record<LogCategory, string[]> = {
  gateway: ['model_id', 'provider', 'input_tokens', 'output_tokens', 'cost_usd', 'latency_ms', 'status_code', 'user_id', 'api_key_id', 'ip_address'],
  mcp: ['tool_name', 'server_name', 'duration_ms', 'status', 'error_message', 'user_id', 'ip_address'],
  audit: ['action', 'resource', 'resource_id', 'user_email', 'user_id', 'ip_address', 'user_agent'],
  platform: ['action', 'resource', 'resource_id', 'user_email', 'user_id', 'ip_address', 'user_agent'],
  access: ['method', 'path', 'status_code', 'latency_ms', 'port', 'user_id', 'ip_address', 'user_agent'],
  app: ['level', 'target', 'message', 'span', 'fields'],
};

// ClickHouse `toString(DateTime64)` returns naive timestamps like
// "2026-04-06 09:21:00.000" without a timezone marker. Browsers parse such
// strings as local time, which is wrong — the value is always UTC. Append a
// "Z" so Date interprets it as UTC, then render in the user's locale.
function formatBackendTimestamp(raw: string): string {
  if (!raw) return '—';
  // Already has timezone info? Use as-is.
  if (/[Zz]|[+-]\d{2}:?\d{2}$/.test(raw)) {
    const d = new Date(raw);
    return Number.isNaN(d.getTime()) ? raw : d.toLocaleString();
  }
  // Naive "YYYY-MM-DD HH:mm:ss[.fff]" — treat as UTC.
  const iso = raw.replace(' ', 'T') + 'Z';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? raw : d.toLocaleString();
}

// Backend returns DateTime64 strings without a timezone suffix, e.g.
// "2026-04-06 09:21:00.000". They are UTC. Without an explicit suffix the
// ---------------------------------------------------------------------------
// LogDetail — per-row expansion content
//
// Renders the most relevant fields for the given log category as a 2-column
// key/value grid, then a collapsible section with the raw JSON for power
// users. The grid uses the DETAIL_HIGHLIGHTS map to decide which keys to
// surface and in what order.
// ---------------------------------------------------------------------------

function formatDetailValue(key: string, raw: unknown): React.ReactNode {
  if (raw === null || raw === undefined || raw === '') return <span className="text-muted-foreground">—</span>;
  if (key === 'cost_usd') return `$${parseFloat(String(raw)).toFixed(6)}`;
  if (key === 'latency_ms' || key === 'duration_ms') return `${raw}ms`;
  if (key === 'created_at' || key === 'timestamp') return formatBackendTimestamp(String(raw));
  if (typeof raw === 'object') {
    return <code className="font-mono text-xs">{JSON.stringify(raw)}</code>;
  }
  const s = String(raw);
  // Long text (e.g. message, span, fields, user_agent) breaks the 2-column
  // grid. Render as preformatted block instead of in a single cell.
  if (s.length > 80) {
    return (
      <pre className="font-mono text-xs whitespace-pre-wrap break-all max-h-40 overflow-y-auto">
        {s}
      </pre>
    );
  }
  return <span className="font-mono text-xs">{s}</span>;
}

function LogDetail({
  log,
  category,
  timeKey,
}: {
  log: LogEntry;
  category: LogCategory;
  timeKey: string;
}) {
  const highlights = DETAIL_HIGHLIGHTS[category];
  // Always include the timestamp first, then the highlight fields, deduped.
  const fields = [timeKey, ...highlights.filter((k) => k !== timeKey)];

  return (
    <div className="space-y-3 p-3">
      <div className="grid grid-cols-1 md:grid-cols-2 gap-x-6 gap-y-2">
        {fields.map((key) => (
          <div key={key} className="flex items-baseline gap-2 text-sm min-w-0">
            <span className="text-xs uppercase tracking-wide text-muted-foreground shrink-0 w-28">
              {key}
            </span>
            <div className="flex-1 min-w-0">{formatDetailValue(key, log[key])}</div>
          </div>
        ))}
      </div>
      <details className="text-xs">
        <summary className="cursor-pointer text-muted-foreground hover:text-foreground select-none">
          Raw JSON
        </summary>
        <pre className="mt-2 rounded bg-muted p-3 font-mono text-xs whitespace-pre-wrap break-all max-h-64 overflow-y-auto">
          {JSON.stringify(log, null, 2)}
        </pre>
      </details>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

function isLogCategory(v: string | undefined): v is LogCategory {
  return (
    v === 'gateway' ||
    v === 'mcp' ||
    v === 'audit' ||
    v === 'platform' ||
    v === 'access' ||
    v === 'app'
  );
}

export function UnifiedLogsPage() {
  const { t } = useTranslation();
  // URL search params are the source of truth for category, query, time
  // range, and page so refreshing or sharing the URL preserves the view.
  const navigate = useNavigate({ from: '/logs' });
  const search = useSearch({ from: '/logs' }) as {
    category?: string;
    q?: string;
    from?: string;
    to?: string;
    page?: number;
  };

  const category: LogCategory = isLogCategory(search.category)
    ? search.category
    : 'platform';
  const activeQuery = search.q ?? '';
  const from = search.from ?? defaultFromLocal();
  const to = search.to ?? defaultToLocal();
  const page = search.page ?? 0;

  // Local-only state: the search input box (committed to URL on Enter / click)
  // and the expanded-row toggle.
  const [searchInput, setSearchInput] = useState(activeQuery);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [expandedRow, setExpandedRow] = useState<string | null>(null);

  // Keep the search input box in sync if the URL changes externally.
  useEffect(() => {
    setSearchInput(activeQuery);
  }, [activeQuery]);

  const updateSearch = useCallback(
    (
      patch: Partial<{
        category: LogCategory;
        q: string;
        from: string;
        to: string;
        page: number;
      }>,
    ) => {
      navigate({
        search: (prev) => {
          const merged = { ...prev, ...patch };
          // Strip empty / default values so the URL stays clean.
          return {
            category: merged.category && merged.category !== 'platform' ? merged.category : undefined,
            q: merged.q || undefined,
            from: merged.from || undefined,
            to: merged.to || undefined,
            page: merged.page && merged.page > 0 ? merged.page : undefined,
          };
        },
        replace: false,
      });
    },
    [navigate],
  );

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const parsed = parseQuery(activeQuery);
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(parsed.params)) {
        if (v) params.set(k, v);
      }
      // Negative tokens (`-key:value`) are joined into a single
      // `exclude=key:value,key:value` param the backend understands.
      if (parsed.excludes.length > 0) {
        params.set('exclude', parsed.excludes.join(','));
      }
      // Convert local wall-clock time to UTC for the backend.
      const utcFrom = localToUtcQuery(from);
      const utcTo = localToUtcQuery(to);
      if (utcFrom) params.set('from', utcFrom);
      if (utcTo) params.set('to', utcTo);
      params.set('limit', String(PAGE_SIZE));
      params.set('offset', String(page * PAGE_SIZE));
      const qs = params.toString();
      const url = `${CATEGORY_API[category]}${qs ? `?${qs}` : ''}`;
      const res = await api<LogsResponse>(url);
      setLogs(res.items ?? []);
      setTotal(res.total ?? 0);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load logs');
      setLogs([]);
      setTotal(0);
    } finally {
      setLoading(false);
    }
  }, [category, activeQuery, from, to, page]);

  useEffect(() => { fetchLogs(); }, [fetchLogs]);

  const handleSearch = () => {
    updateSearch({ q: searchInput, page: 0 });
  };

  /**
   * Append a `key:value` (or `-key:value`) token to the active query and
   * re-search. If a positive `key:` token is already present, it is replaced
   * so users can click "+" on different rows to switch the filter instead
   * of stacking duplicates. Negative tokens with the same key+value are
   * also de-duplicated.
   */
  const updateFilter = (key: string, rawValue: unknown, negate: boolean) => {
    if (rawValue === null || rawValue === undefined || rawValue === '') return;
    let value = String(rawValue);
    if (/\s/.test(value)) value = `"${value.replace(/"/g, '\\"')}"`;
    const token = `${negate ? '-' : ''}${key}:${value}`;

    // Strip any existing positive `key:...` token (only one allowed at a time).
    let stripped = activeQuery.replace(
      new RegExp(`(?<![-\\w])${key}:(?:"[^"]*"|\\S+)\\s*`, 'g'),
      '',
    );
    // Also strip a duplicate of the exact token we are about to add (for
    // negatives, so clicking "−" twice on the same row is a no-op).
    stripped = stripped
      .replace(
        new RegExp(`\\B${escapeRegex(token)}(?:\\s|$)`, 'g'),
        '',
      )
      .trim();

    const next = stripped ? `${stripped} ${token}` : token;
    setSearchInput(next);
    updateSearch({ q: next, page: 0 });
  };

  const handleAddFilter = (key: string, rawValue: unknown) =>
    updateFilter(key, rawValue, false);
  const handleExcludeFilter = (key: string, rawValue: unknown) =>
    updateFilter(key, rawValue, true);

  const handleCategoryChange = (v: string) => {
    if (!isLogCategory(v)) return;
    setExpandedRow(null);
    updateSearch({ category: v, page: 0 });
  };

  const setFrom = (v: string) => updateSearch({ from: v, page: 0 });
  const setTo = (v: string) => updateSearch({ to: v, page: 0 });
  const setPage = (p: number) => updateSearch({ page: p });

  const totalPages = Math.ceil(total / PAGE_SIZE);
  const columns = getColumns(category);
  const timeKey = getTimeKey(category);

  const placeholders: Record<LogCategory, string> = {
    gateway: 'model:gpt-4o status_code:200 provider:openai  (or just: gpt-4o)',
    mcp: 'tool_name:search status:error  (or just: search)',
    audit: 'action:create resource:provider user_id:xxx',
    platform: 'action:role.deleted resource:role user_id:xxx',
    access: 'method:POST path:/api/admin status_code:500  (or just: /admin)',
    app: 'level:error target:auth  (or any text from the message)',
  };

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('unifiedLogs.title', 'Logs')}</h1>
        <p className="text-muted-foreground">{t('unifiedLogs.subtitle', 'Unified log explorer')}</p>
      </div>

      <div className="flex gap-2 items-center">
        <Select value={category} onValueChange={handleCategoryChange}>
          <SelectTrigger className="w-40 shrink-0">
            {/* Show only the short label in the closed trigger */}
            <span className="truncate">{t(`unifiedLogs.${category}`)}</span>
          </SelectTrigger>
          <SelectContent className="max-w-sm">
            {(['platform', 'audit', 'gateway', 'mcp', 'access', 'app'] as const).map((cat) => (
              <SelectItem key={cat} value={cat} className="py-2">
                <div className="flex flex-col gap-0.5">
                  <span className="font-medium">{t(`unifiedLogs.${cat}`)}</span>
                  <span className="text-xs text-muted-foreground">
                    {t(`unifiedLogs.${cat}Desc`)}
                  </span>
                </div>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Input
          placeholder={placeholders[category]}
          value={searchInput}
          onChange={(e) => setSearchInput(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && handleSearch()}
          className="flex-1 font-mono text-sm"
        />
        <DateTimeRangePicker
          className="shrink-0"
          from={from}
          to={to}
          onFromChange={setFrom}
          onToChange={setTo}
        />
        <Button onClick={handleSearch} className="shrink-0">
          <Search className="h-4 w-4 mr-1" />
          {t('common.search')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between py-3">
          <CardTitle className="text-base">{t('unifiedLogs.results', 'Results')}</CardTitle>
          {total > 0 && (
            <span className="text-sm text-muted-foreground">{t('common.total')}: {total.toLocaleString()}</span>
          )}
        </CardHeader>
        <CardContent className="p-0">
          {loading ? (
            <div className="space-y-3 p-6">
              {[...Array(5)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-8" />
                  <Skeleton className="h-4 w-28" />
                  <Skeleton className="h-4 w-36" />
                  <Skeleton className="h-4 w-24" />
                </div>
              ))}
            </div>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-16 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('unifiedLogs.noLogs', 'No logs found.')}</p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-8" />
                    {columns.map((col) => (
                      <TableHead key={col.key} className={col.align === 'right' ? 'text-right' : ''}>
                        {col.label}
                      </TableHead>
                    ))}
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {logs.map((log) => {
                    const rowTime = String(log[timeKey] ?? log.created_at ?? '');
                    return (
                      <>
                        <TableRow key={log.id}>
                          <TableCell>
                            <Button variant="ghost" size="icon-xs" aria-label="Expand"
                              onClick={() => setExpandedRow(expandedRow === log.id ? null : log.id)}>
                              {expandedRow === log.id
                                ? <ChevronDown className="h-3 w-3" />
                                : <ChevronRight className="h-3 w-3" />}
                            </Button>
                          </TableCell>
                          {columns.map((col) => {
                            const val = log[col.key];
                            let display: React.ReactNode;
                            if (col.render) {
                              display = col.render(val, log);
                            } else if (col.key === timeKey || col.key === 'created_at' || col.key === 'timestamp') {
                              display = formatBackendTimestamp(rowTime);
                            } else {
                              display = val != null ? String(val) : '—';
                            }
                            // Click "+" to filter by this cell's value.
                            // Some columns display one field but filter on a
                            // different one (e.g. user_email column → user_id).
                            const filterValue = col.filterValueKey
                              ? log[col.filterValueKey]
                              : val;
                            const isFilterable =
                              !!col.filterKey &&
                              filterValue !== null &&
                              filterValue !== undefined &&
                              filterValue !== '';
                            return (
                              <TableCell key={col.key}
                                className={`text-sm ${col.align === 'right' ? 'text-right tabular-nums' : ''} ${col.mono ? 'font-mono' : ''}`}>
                                <div className="group/cell flex items-center gap-1">
                                  <span className="min-w-0">{display}</span>
                                  {isFilterable && (
                                    <span className="flex shrink-0 items-center gap-0.5">
                                      <button
                                        type="button"
                                        title={`Filter: ${col.filterKey}:${filterValue}`}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleAddFilter(col.filterKey!, filterValue);
                                        }}
                                        className="opacity-0 group-hover/cell:opacity-60 hover:!opacity-100 hover:bg-accent rounded p-0.5 transition-opacity"
                                      >
                                        <Plus className="h-3 w-3" />
                                      </button>
                                      <button
                                        type="button"
                                        title={`Exclude: -${col.filterKey}:${filterValue}`}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleExcludeFilter(col.filterKey!, filterValue);
                                        }}
                                        className="opacity-0 group-hover/cell:opacity-60 hover:!opacity-100 hover:bg-accent rounded p-0.5 transition-opacity"
                                      >
                                        <Minus className="h-3 w-3" />
                                      </button>
                                    </span>
                                  )}
                                </div>
                              </TableCell>
                            );
                          })}
                        </TableRow>
                        {expandedRow === log.id && (
                          <TableRow key={`${log.id}-detail`}>
                            <TableCell colSpan={columns.length + 1} className="bg-muted/30">
                              <LogDetail log={log} category={category} timeKey={timeKey} />
                            </TableCell>
                          </TableRow>
                        )}
                      </>
                    );
                  })}
                </TableBody>
              </Table>
              {totalPages > 1 && (
                <div className="flex items-center justify-between p-4 border-t">
                  <span className="text-sm text-muted-foreground">
                    {page * PAGE_SIZE + 1}–{Math.min((page + 1) * PAGE_SIZE, total)} / {total}
                  </span>
                  <Pagination className="mx-0 w-auto">
                    <PaginationContent>
                      <PaginationItem>
                        <PaginationPrevious text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page - 1); }}
                          aria-disabled={page === 0}
                          className={page === 0 ? 'pointer-events-none opacity-50' : ''} />
                      </PaginationItem>
                      <PaginationItem>
                        <span className="text-sm px-2">{page + 1} / {totalPages}</span>
                      </PaginationItem>
                      <PaginationItem>
                        <PaginationNext text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page + 1); }}
                          aria-disabled={page >= totalPages - 1}
                          className={page >= totalPages - 1 ? 'pointer-events-none opacity-50' : ''} />
                      </PaginationItem>
                    </PaginationContent>
                  </Pagination>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
