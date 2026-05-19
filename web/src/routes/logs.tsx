import React, { Fragment, useEffect, useMemo, useState, useCallback, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { subHours, format } from 'date-fns';
import Decimal from 'decimal.js';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import { Select, SelectContent, SelectItem, SelectTrigger } from '@/components/ui/select';
import { Search, FileText, ChevronDown, ChevronRight, Plus, Minus, X } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api, hasPermission } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';
import { Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious } from '@/components/ui/pagination';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type LogCategory = 'gateway' | 'mcp' | 'audit' | 'access' | 'app';

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
  access: '/api/admin/access-logs',
  app: '/api/admin/app-logs',
};

const PAGE_SIZE = 50;

import { escapeRegex, parseQuery, removeFilterToken } from './logs/query-parser';

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

// Render parsed `key:value` and `-key:value` tokens as removable pills right
// below the search row, so the user can see which filters the backend will
// actually apply. Non-filter free text is intentionally left out — it stays
// visible in the Input itself. × rewrites `input` to drop that one token.
function QueryTokenChips({
  input,
  onChange,
}: {
  input: string;
  onChange: (next: string) => void;
}) {
  const { params, excludes } = parseQuery(input);
  type Chip = { key: string; value: string; negate: boolean };
  const chips: Chip[] = [];
  for (const [k, v] of Object.entries(params)) {
    if (k === 'q') continue;
    chips.push({ key: k, value: v, negate: false });
  }
  for (const raw of excludes) {
    const idx = raw.indexOf(':');
    if (idx <= 0) continue;
    const key = raw.slice(0, idx);
    const rawVal = raw.slice(idx + 1);
    // Unquote for display only. parseQuery already normalized the shape.
    const value =
      rawVal.startsWith('"') && rawVal.endsWith('"')
        ? rawVal.slice(1, -1).replace(/\\"/g, '"').replace(/\\\\/g, '\\')
        : rawVal;
    chips.push({ key, value, negate: true });
  }
  if (chips.length === 0) return null;
  return (
    <div className="-mt-2 mb-4 flex flex-wrap items-center gap-1.5">
      {chips.map((c) => (
        <span
          key={`${c.negate ? '-' : '+'}${c.key}:${c.value}`}
          className={
            c.negate
              ? 'inline-flex items-center gap-1 rounded border border-destructive/40 bg-destructive/10 px-1.5 py-0.5 font-mono text-[11px] text-destructive'
              : 'inline-flex items-center gap-1 rounded border border-primary/40 bg-primary/10 px-1.5 py-0.5 font-mono text-[11px] text-primary'
          }
        >
          {c.negate ? '-' : ''}
          {c.key}:{c.value}
          <button
            type="button"
            aria-label={`Remove ${c.negate ? '-' : ''}${c.key}:${c.value}`}
            onClick={() => onChange(removeFilterToken(input, c.key, c.negate))}
            className="rounded p-0.5 hover:bg-background/60"
          >
            <X className="h-3 w-3" aria-hidden="true" />
          </button>
        </span>
      ))}
    </div>
  );
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

function getColumns(cat: LogCategory, t: (key: string) => string): ColDef[] {
  // Column labels go through `logs.col.*` keys so the unified Logs
  // page is i18n-clean (zh translations were silently ignored when
  // the labels were hardcoded English strings).
  const T = (k: string) => t(`logs.col.${k}`);
  switch (cat) {
    case 'gateway':
      return [
        { key: 'created_at', label: T('time') },
        { key: 'model_id', label: T('model'), mono: true, filterKey: 'model' },
        { key: 'provider', label: T('provider'), filterKey: 'provider' },
        { key: 'upstream_model', label: T('upstream'), mono: true, filterKey: 'upstream_model' },
        { key: 'input_tokens', label: T('in'), align: 'right' },
        { key: 'output_tokens', label: T('out'), align: 'right' },
        // cost_usd arrives as a Decimal string from CH; parseFloat
        // would silently round long fractional values. Use Decimal.js
        // so the displayed number matches the audit trail bit-for-bit.
        { key: 'cost_usd', label: T('cost'), align: 'right', render: (v) => `$${new Decimal(String(v ?? 0)).toFixed(4)}` },
        { key: 'latency_ms', label: T('latency'), align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status_code', label: T('status'), render: (v) => statusBadge(v), filterKey: 'status_code' },
      ];
    case 'mcp':
      return [
        { key: 'created_at', label: T('time') },
        { key: 'tool_name', label: T('tool'), mono: true, filterKey: 'tool_name' },
        { key: 'server_name', label: T('server'), filterKey: 'server_id', filterValueKey: 'server_id' },
        { key: 'duration_ms', label: T('duration'), align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status', label: T('status'), render: (v) => <Badge variant={v === 'success' ? 'default' : 'destructive'}>{String(v)}</Badge>, filterKey: 'status' },
        { key: 'user_email', label: T('user'), filterKey: 'user_id', filterValueKey: 'user_id' },
      ];
    case 'audit':
      return [
        { key: 'created_at', label: T('time') },
        { key: 'user_email', label: T('user'), filterKey: 'user_id', filterValueKey: 'user_id' },
        { key: 'api_key_id', label: T('apiKeyId'), mono: true, filterKey: 'api_key_id' },
        { key: 'action', label: T('action'), filterKey: 'action' },
        { key: 'resource', label: T('resource'), filterKey: 'resource' },
        { key: 'ip_address', label: T('ip'), mono: true },
      ];
    case 'access':
      return [
        { key: 'created_at', label: T('time') },
        { key: 'method', label: T('method'), filterKey: 'method' },
        { key: 'path', label: T('path'), mono: true, filterKey: 'path' },
        { key: 'status_code', label: T('status'), render: (v) => statusBadge(v), filterKey: 'status_code' },
        { key: 'latency_ms', label: T('latency'), align: 'right', render: (v) => `${v}ms` },
        { key: 'port', label: T('port'), filterKey: 'port' },
        { key: 'ip_address', label: T('ip'), mono: true },
      ];
    case 'app':
      return [
        { key: 'created_at', label: T('time') },
        { key: 'level', label: T('level'), render: (v) => levelBadge(v), filterKey: 'level' },
        { key: 'target', label: T('target'), mono: true, filterKey: 'target' },
        { key: 'message', label: T('message') },
        { key: 'span', label: T('span') },
      ];
  }
}

function getTimeKey(_cat: LogCategory): string {
  // All log categories use `created_at` as the timestamp field.
  return 'created_at';
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

/// Add two `Option<number>` cells from a log row into a single
/// total. Returns `null` only when BOTH inputs are absent — a single
/// captured side is enough to surface the size hint.
function sumNullable(a: unknown, b: unknown): number | null {
  const num = (v: unknown): number => (typeof v === 'number' ? v : 0);
  if (a == null && b == null) return null;
  return num(a) + num(b);
}

/// Pick the right i18n unit for a byte count. Capped at MB because
/// `audit.body_max_bytes` defaults to 256 KiB and 99% of captures
/// sit well under 10 MB even with offload — no need for GB scale.
function formatBytes(t: (key: string, opts?: Record<string, unknown>) => string, bytes: number): string {
  if (bytes >= 1_000_000) {
    return t('logs.bodies.sizeMb', { mb: (bytes / 1_000_000).toFixed(1) });
  }
  if (bytes >= 1_000) {
    return t('logs.bodies.sizeKb', { kb: (bytes / 1_000).toFixed(1) });
  }
  return t('logs.bodies.size', { bytes: bytes.toLocaleString() });
}

// "Highlight" fields rendered above the raw JSON in the per-row expansion.
// Order matters; the first listed fields show first.
const DETAIL_HIGHLIGHTS: Record<LogCategory, string[]> = {
  gateway: ['model_id', 'provider', 'upstream_model', 'input_tokens', 'output_tokens', 'cost_usd', 'latency_ms', 'status_code', 'user_id', 'api_key_id', 'ip_address'],
  mcp: ['tool_name', 'server_name', 'duration_ms', 'status', 'error_message', 'user_id', 'ip_address'],
  audit: ['action', 'resource', 'resource_id', 'user_email', 'user_id', 'api_key_id', 'ip_address', 'user_agent', 'detail'],
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
  if (key === 'cost_usd') return `$${new Decimal(String(raw)).toFixed(6)}`;
  if (key === 'latency_ms' || key === 'duration_ms') return `${raw}ms`;
  if (key === 'created_at' || key === 'timestamp') return formatBackendTimestamp(String(raw));
  // Audit `detail` is a structured who-changed-what-from-X-to-Y blob. A flat
  // JSON.stringify is unreadable; pretty-print it so reviewers can scan
  // multi-key diffs at a glance. Other object fields keep the compact form.
  if (key === 'detail' && typeof raw === 'object') {
    return <PrettyJsonBlock value={raw} />;
  }
  if (typeof raw === 'object') {
    return <ObjectDetailValue value={raw} />;
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

// Stringify off the render path: the same object identity flowing
// through detail re-renders (table sort, hover state, anything that
// re-renders the parent row) used to JSON.stringify on every pass.
// memo + useMemo keep both the React element and the JSON string
// stable as long as the value reference doesn't change.
const ObjectDetailValue = React.memo(function ObjectDetailValue({ value }: { value: unknown }) {
  const text = useMemo(() => JSON.stringify(value), [value]);
  return <code className="font-mono text-xs">{text}</code>;
});

// Pretty-printed JSON for the audit `detail` field. Kept separate from
// RawJsonBlock so the 2-column grid cell can host it without inheriting
// the outer card's background, and so a huge detail (e.g. a large policy
// diff) doesn't dominate the row — the max-height caps it at ~16rem.
const PrettyJsonBlock = React.memo(function PrettyJsonBlock({ value }: { value: unknown }) {
  const text = useMemo(() => JSON.stringify(value, null, 2), [value]);
  return (
    <pre className="rounded bg-muted/60 p-2 font-mono text-xs whitespace-pre-wrap break-all max-h-64 overflow-auto">
      {text}
    </pre>
  );
});

const RawJsonBlock = React.memo(function RawJsonBlock({ value }: { value: unknown }) {
  const text = useMemo(() => JSON.stringify(value, null, 2), [value]);
  return (
    <pre className="mt-2 rounded bg-muted p-3 font-mono text-xs whitespace-pre-wrap break-all max-h-64 overflow-y-auto">
      {text}
    </pre>
  );
});

function LogDetail({
  log,
  category,
  timeKey,
}: {
  log: LogEntry;
  category: LogCategory;
  timeKey: string;
}) {
  const { t } = useTranslation();
  const highlights = DETAIL_HIGHLIGHTS[category];
  // Always include the timestamp first, then the highlight fields, deduped.
  // The audit `detail` blob renders as a full-width section below the grid
  // (it's too tall to fit a 2-col cell cleanly) — drop it from the grid here.
  const gridFields = [timeKey, ...highlights.filter((k) => k !== timeKey && k !== 'detail')];
  const showAuditDetail =
    category === 'audit' &&
    log.detail !== null &&
    log.detail !== undefined &&
    typeof log.detail === 'object';

  // Bodies live in a separate admin endpoint behind `logs:read_bodies`
  // — render the viewer only on gateway/mcp rows for users that
  // actually hold the permission. Each fetch fires a server-side
  // `audit.body_viewed` row, so we want the user to think before
  // clicking — surface the body size on the button label so the
  // auditor knows whether they're about to fetch 5 KB or 5 MB.
  const showBodyViewer =
    (category === 'gateway' || category === 'mcp') && hasPermission('logs:read_bodies');
  // Read the size hint from the list-row payload so the button can
  // render "(2.3 MB)" before the auditor clicks. Falls back to no
  // suffix when the row predates the column (NULL on old data).
  const totalBodyBytes: number | null = showBodyViewer
    ? (category === 'gateway'
        ? sumNullable(log.request_body_bytes, log.response_body_bytes)
        : sumNullable(log.arguments_bytes, log.result_bytes))
    : null;

  return (
    <div className="space-y-3 p-3">
      <div className="grid grid-cols-1 md:grid-cols-2 gap-x-6 gap-y-2">
        {gridFields.map((key) => (
          <div key={key} className="flex items-baseline gap-2 text-sm min-w-0">
            <span className="text-xs uppercase tracking-wide text-muted-foreground shrink-0 w-28">
              {key}
            </span>
            <div className="flex-1 min-w-0">{formatDetailValue(key, log[key])}</div>
          </div>
        ))}
      </div>
      {showAuditDetail && (
        <div className="space-y-1">
          <div className="text-xs uppercase tracking-wide text-muted-foreground">
            {t('logs.audit.detailJson')}
          </div>
          <PrettyJsonBlock value={log.detail} />
        </div>
      )}
      {showBodyViewer && (
        <BodyViewer
          category={category}
          logId={String(log.id ?? '')}
          totalBytes={totalBodyBytes}
        />
      )}
      <Collapsible className="text-xs">
        <CollapsibleTrigger className="cursor-pointer text-muted-foreground hover:text-foreground select-none">
          {t('logs.rawJson')}
        </CollapsibleTrigger>
        <CollapsibleContent>
          <RawJsonBlock value={log} />
        </CollapsibleContent>
      </Collapsible>
    </div>
  );
}

// ---------------------------------------------------------------------------
// BodyViewer — fetch + render the captured request/response payloads.
// ---------------------------------------------------------------------------
//
// Renders a click-to-load button (NOT a Collapsible that auto-fetches on
// open) because every successful fetch emits a server-side
// `audit.body_viewed` row. We don't want an idle tab + auto-expand to
// spam the audit log with phantom "viewed" entries — surface it as an
// explicit user action.

interface GatewayBodyResponse {
  id: string;
  trace_id: string | null;
  user_id: string | null;
  model_id: string | null;
  created_at: string;
  request_body: string | null;
  response_body: string | null;
  request_body_bytes: number | null;
  response_body_bytes: number | null;
  body_capture_status: string | null;
}

interface McpBodyResponse {
  id: string;
  trace_id: string | null;
  user_id: string | null;
  server_id: string | null;
  server_name: string | null;
  tool_name: string | null;
  created_at: string;
  tool_arguments: string | null;
  tool_result: string | null;
  arguments_bytes: number | null;
  result_bytes: number | null;
  body_capture_status: string | null;
}

type BodyState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'ok'; data: GatewayBodyResponse | McpBodyResponse }
  | { kind: 'err'; msg: string };

function BodyViewer({
  category,
  logId,
  totalBytes,
}: {
  category: 'gateway' | 'mcp';
  logId: string;
  totalBytes: number | null;
}) {
  const { t } = useTranslation();
  const [state, setState] = useState<BodyState>({ kind: 'idle' });

  const fetchBody = useCallback(() => {
    if (state.kind === 'loading' || state.kind === 'ok') return;
    setState({ kind: 'loading' });
    const path =
      category === 'gateway'
        ? `/api/admin/gateway/logs/${encodeURIComponent(logId)}/body`
        : `/api/admin/mcp/logs/${encodeURIComponent(logId)}/body`;
    api<GatewayBodyResponse | McpBodyResponse>(path)
      .then((data) => setState({ kind: 'ok', data }))
      .catch((err: unknown) =>
        setState({
          kind: 'err',
          msg: err instanceof Error ? err.message : 'unknown',
        }),
      );
  }, [category, logId, state.kind]);

  if (state.kind === 'idle') {
    const sizeLabel = totalBytes != null ? formatBytes(t, totalBytes) : null;
    const buttonLabel = sizeLabel
      ? t(category === 'gateway' ? 'logs.bodies.viewWithSize' : 'logs.bodies.viewMcpWithSize', {
          size: sizeLabel,
        })
      : t(category === 'gateway' ? 'logs.bodies.view' : 'logs.bodies.viewMcp');
    return (
      <div className="space-y-1 rounded border border-dashed border-muted-foreground/40 p-2">
        <Button
          size="sm"
          variant="outline"
          onClick={fetchBody}
          aria-label={buttonLabel}>
          {buttonLabel}
        </Button>
        <p className="text-xs text-muted-foreground">{t('logs.bodies.permRequired')}</p>
      </div>
    );
  }
  if (state.kind === 'loading') {
    return <div className="text-xs text-muted-foreground">{t('logs.bodies.loading')}</div>;
  }
  if (state.kind === 'err') {
    return (
      <Alert variant="destructive">
        <AlertDescription>{t('logs.bodies.fetchFailed', { msg: state.msg })}</AlertDescription>
      </Alert>
    );
  }

  const status = state.data.body_capture_status;
  const statusLabel =
    status === 'captured'
      ? t('logs.bodies.statusCaptured')
      : status === 'truncated'
        ? t('logs.bodies.statusTruncated')
        : status === 'disabled'
          ? t('logs.bodies.statusDisabled')
          : status === 'from_cache'
            ? t('logs.bodies.statusFromCache')
            : status === 'offloaded'
              ? t('logs.bodies.statusOffloaded')
              : status === 'error'
                ? t('logs.bodies.statusError')
                : null;

  const [reqLabel, respLabel, reqBody, respBody, reqBytes, respBytes] =
    category === 'gateway'
      ? [
          t('logs.bodies.request'),
          t('logs.bodies.response'),
          (state.data as GatewayBodyResponse).request_body,
          (state.data as GatewayBodyResponse).response_body,
          (state.data as GatewayBodyResponse).request_body_bytes,
          (state.data as GatewayBodyResponse).response_body_bytes,
        ]
      : [
          t('logs.bodies.arguments'),
          t('logs.bodies.result'),
          (state.data as McpBodyResponse).tool_arguments,
          (state.data as McpBodyResponse).tool_result,
          (state.data as McpBodyResponse).arguments_bytes,
          (state.data as McpBodyResponse).result_bytes,
        ];

  return (
    <div className="space-y-2 rounded border p-2">
      {statusLabel && (
        <div className="text-xs text-muted-foreground italic">{statusLabel}</div>
      )}
      <BodyPanel label={reqLabel} body={reqBody} bytes={reqBytes} downloadName={`${logId}-request.json`} />
      <BodyPanel label={respLabel} body={respBody} bytes={respBytes} downloadName={`${logId}-response.json`} />
    </div>
  );
}

function BodyPanel({
  label,
  body,
  bytes,
  downloadName,
}: {
  label: string;
  body: string | null;
  bytes: number | null;
  downloadName: string;
}) {
  const { t } = useTranslation();
  // Try to pretty-print as JSON; fall back to raw text if it's not parseable
  // (e.g. truncated mid-token).
  let display = body ?? '';
  if (body) {
    try {
      display = JSON.stringify(JSON.parse(body), null, 2);
    } catch {
      // Keep raw — truncated bodies legitimately don't parse.
    }
  }
  const handleDownload = useCallback(() => {
    if (!body) return;
    // Browser blob download. Use the RAW (un-pretty-printed) body so
    // the saved file matches the audit row's stored content byte-for-
    // byte — auditors taking evidence off the wire want bit-fidelity.
    const blob = new Blob([body], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = downloadName;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  }, [body, downloadName]);
  return (
    <div className="space-y-1">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs uppercase tracking-wide text-muted-foreground">{label}</span>
        <div className="flex items-baseline gap-2">
          {bytes != null && (
            <span className="text-xs text-muted-foreground tabular-nums">
              {t('logs.bodies.size', { bytes: bytes.toLocaleString() })}
            </span>
          )}
          {body && (
            <Button size="sm" variant="ghost" className="h-6 px-2 text-xs" onClick={handleDownload}>
              {t('logs.bodies.download')}
            </Button>
          )}
        </div>
      </div>
      {body == null || body === '' ? (
        <div className="text-xs text-muted-foreground italic">{t('logs.bodies.noBody')}</div>
      ) : (
        <pre className="max-h-96 overflow-auto rounded bg-muted/40 p-2 text-xs">
          <code>{display}</code>
        </pre>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

function isLogCategory(v: string | undefined): v is LogCategory {
  return v === 'gateway' || v === 'mcp' || v === 'audit' || v === 'access' || v === 'app';
}

export function UnifiedLogsPage() {
  const { t } = useTranslation();
  // URL search params are the source of truth for category, query, time
  // range, and page so refreshing or sharing the URL preserves the view.
  const navigate = useNavigate({ from: '/logs' });
  // Typed via the route's `validateSearch` in router.tsx. No cast —
  // if the route's search shape changes, `useSearch` here becomes
  // a compile error and we get told instead of silently drifting.
  const search = useSearch({ from: '/logs' });

  const category: LogCategory = isLogCategory(search.category) ? search.category : 'audit';
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
  // Monotonic id stamped on every fetch so a slow response from the
  // previous category can't overwrite the latest result. Without this,
  // switching from "访问日志" (14k rows) to "网关日志" (4 rows) shows
  // 543 access-log-shaped rows under gateway column headers for the
  // duration of the in-flight access request — the exact "the data
  // doesn't match the label" bug operators hit.
  const fetchTokenRef = useRef(0);

  // Mirror the active query into a ref so we can sync the input *only*
  // when the URL truly changes from somewhere else (browser back/forward,
  // external navigation), not on every re-render. Without this, URL
  // updates from our own handleSearch would overwrite local state,
  // racing the user's next keystroke.
  const lastSyncedQueryRef = useRef(activeQuery);
  useEffect(() => {
    if (activeQuery !== lastSyncedQueryRef.current) {
      lastSyncedQueryRef.current = activeQuery;
      setSearchInput(activeQuery);
    }
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
            category: merged.category && merged.category !== 'audit' ? merged.category : undefined,
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
    // Stamp this attempt and clear stale rows BEFORE the network call
    // so the skeleton — not the previous category's data — bridges the
    // request window.
    const myToken = ++fetchTokenRef.current;
    setLogs([]);
    setTotal(0);
    setExpandedRow(null);
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
      // Drop a stale response if the user has since switched categories
      // or fired another search — the newer fetch is now authoritative.
      if (fetchTokenRef.current !== myToken) return;
      setLogs(res.items ?? []);
      setTotal(res.total ?? 0);
    } catch (err) {
      if (fetchTokenRef.current !== myToken) return;
      setError(err instanceof Error ? err.message : t('common.error'));
      setLogs([]);
      setTotal(0);
    } finally {
      if (fetchTokenRef.current === myToken) setLoading(false);
    }
  }, [category, activeQuery, from, to, page, t]);

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
  const updateFilter = useCallback(
    (key: string, rawValue: unknown, negate: boolean) => {
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
    },
    [activeQuery, updateSearch],
  );

  // Stable references so memoized children (LogRow) don't re-render every
  // time the parent's state changes unrelated to filters.
  const handleAddFilter = useCallback(
    (key: string, rawValue: unknown) => updateFilter(key, rawValue, false),
    [updateFilter],
  );
  const handleExcludeFilter = useCallback(
    (key: string, rawValue: unknown) => updateFilter(key, rawValue, true),
    [updateFilter],
  );

  const handleCategoryChange = (v: string) => {
    if (!isLogCategory(v)) return;
    setExpandedRow(null);
    updateSearch({ category: v, page: 0 });
  };

  const setFrom = (v: string) => updateSearch({ from: v, page: 0 });
  const setTo = (v: string) => updateSearch({ to: v, page: 0 });
  const setPage = (p: number) => updateSearch({ page: p });

  const totalPages = Math.ceil(total / PAGE_SIZE);
  const columns = getColumns(category, t);
  const timeKey = getTimeKey(category);

  // Hint lives below the input as a tertiary, italicized example so the
  // input itself shows a neutral "Search…" — the previous full-syntax
  // placeholder looked indistinguishable from an applied filter and led
  // operators to chase "why is 101 in my status:200 results?" when in
  // fact no filter was applied.
  const syntaxHints: Record<LogCategory, string> = {
    gateway: 'model:gpt-4o provider:openai status_code:200',
    mcp: 'tool_name:search status:error',
    audit: 'action:provider.created resource:provider',
    access: 'method:POST path:/api/admin status_code:500',
    app: 'level:error target:auth',
  };

  // Count chips so we can tell the user explicitly when nothing is
  // narrowing the result set — silent "0 chips" + a non-empty results
  // table is the exact failure mode that triggered this fix.
  const parsedActive = parseQuery(searchInput);
  const activeFilterCount =
    Object.keys(parsedActive.params).filter((k) => k !== 'q').length +
    parsedActive.excludes.length +
    (parsedActive.params.q ? 1 : 0);

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="mb-4">
        <h1 className="text-2xl font-semibold tracking-tight">{t('unifiedLogs.title', 'Logs')}</h1>
        <p className="text-muted-foreground">{t('unifiedLogs.subtitle', 'Unified log explorer')}</p>
      </div>

      <div className="flex gap-2 items-center mb-4">
        <Select value={category} onValueChange={handleCategoryChange}>
          <SelectTrigger className="w-40 shrink-0">
            {/* Show only the short label in the closed trigger */}
            <span className="truncate">{t(`unifiedLogs.${category}`)}</span>
          </SelectTrigger>
          <SelectContent className="max-w-sm">
            {(['audit', 'gateway', 'mcp', 'access', 'app'] as const).map((cat) => (
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
          placeholder={t('logs.searchPlaceholder', 'Search…')}
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

      <div className="-mt-1 mb-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[10px] text-muted-foreground">
        <span>{t('logs.utcNotice')}</span>
        <span className="opacity-70">
          {t('logs.syntaxHintLabel', 'Try')}: <code className="font-mono">{syntaxHints[category]}</code>
        </span>
        {activeFilterCount === 0 ? (
          <span className="rounded border border-amber-500/40 bg-amber-500/10 px-1.5 py-0.5 font-medium text-amber-700 dark:text-amber-300">
            {t('logs.noFilterShowingAll', 'No filter — showing all logs')}
            {total > 0 && ` · ${total.toLocaleString()} ${t('logs.totalCountSuffix', 'results')}`}
          </span>
        ) : (
          // When filters ARE active, surface a one-line summary so the
          // user can confirm "yes, my filter applied, and it matched N
          // out of the time window" without scrolling to the pagination.
          <span className="rounded border border-primary/40 bg-primary/10 px-1.5 py-0.5 font-medium text-primary">
            {t('logs.filterSummary', {
              count: activeFilterCount,
              matches: total.toLocaleString(),
            })}
          </span>
        )}
      </div>

      <QueryTokenChips input={searchInput} onChange={setSearchInput} />

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
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
            <div className="flex h-full flex-col items-center justify-center text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('unifiedLogs.noLogs', 'No logs found.')}</p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
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
                      <Fragment key={log.id}>
                        <TableRow>
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
                                        aria-label={`Add filter ${col.filterKey}=${filterValue}`}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleAddFilter(col.filterKey!, filterValue);
                                        }}
                                        className="opacity-0 group-hover/cell:opacity-60 hover:!opacity-100 hover:bg-accent rounded p-0.5 transition-opacity"
                                      >
                                        <Plus className="h-3 w-3" aria-hidden="true" />
                                      </button>
                                      <button
                                        type="button"
                                        title={`Exclude: -${col.filterKey}:${filterValue}`}
                                        aria-label={`Exclude filter ${col.filterKey}=${filterValue}`}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleExcludeFilter(col.filterKey!, filterValue);
                                        }}
                                        className="opacity-0 group-hover/cell:opacity-60 hover:!opacity-100 hover:bg-accent rounded p-0.5 transition-opacity"
                                      >
                                        <Minus className="h-3 w-3" aria-hidden="true" />
                                      </button>
                                    </span>
                                  )}
                                </div>
                              </TableCell>
                            );
                          })}
                        </TableRow>
                        {expandedRow === log.id && (
                          <TableRow>
                            <TableCell colSpan={columns.length + 1} className="bg-muted/30">
                              <LogDetail log={log} category={category} timeKey={timeKey} />
                            </TableCell>
                          </TableRow>
                        )}
                      </Fragment>
                    );
                  })}
                </TableBody>
              </Table>
            </>
          )}
        </CardContent>
        <div data-slot="card-footer" className="flex items-center justify-between border-t px-3 py-1.5">
          <span className="text-xs text-muted-foreground">
            {total === 0
              ? '0'
              : `${page * PAGE_SIZE + 1}–${Math.min((page + 1) * PAGE_SIZE, total)} / ${total}`}
          </span>
          <Pagination className="mx-0 w-auto">
            <PaginationContent className="gap-0.5">
              <PaginationItem>
                <PaginationPrevious text="" size="icon"
                  className={`h-7 w-7 ${page === 0 ? 'pointer-events-none opacity-50' : ''}`}
                  onClick={(e: React.MouseEvent) => { e.preventDefault(); if (page > 0) setPage(page - 1); }}
                  aria-disabled={page === 0} />
              </PaginationItem>
              <PaginationItem>
                <span className="px-2 text-xs">{page + 1} / {Math.max(totalPages, 1)}</span>
              </PaginationItem>
              <PaginationItem>
                <PaginationNext text="" size="icon"
                  className={`h-7 w-7 ${page >= totalPages - 1 ? 'pointer-events-none opacity-50' : ''}`}
                  onClick={(e: React.MouseEvent) => { e.preventDefault(); if (page < totalPages - 1) setPage(page + 1); }}
                  aria-disabled={page >= totalPages - 1} />
              </PaginationItem>
            </PaginationContent>
          </Pagination>
        </div>
      </Card>
    </div>
  );
}
