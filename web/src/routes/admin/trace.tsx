import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useParams } from '@tanstack/react-router';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertCircle, Search } from 'lucide-react';
import { api } from '@/lib/api';

interface TraceEvent {
  kind: 'gateway' | 'mcp' | 'audit';
  id: string;
  created_at: string;
  subject: string;
  status: string;
  duration_ms: number;
  user_id: string | null;
}

interface TraceResponse {
  trace_id: string;
  events: TraceEvent[];
}

/// Admin-only page that shows every event logged for a given trace_id.
/// The trace_id is the value returned in the `x-trace-id` response
/// header on any API call — operators paste it here to see the
/// chronological fan-out of a single request across the three log
/// tables (gateway_logs, mcp_logs, audit_logs).
export function TracePage() {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { traceId?: string };
  const [traceInput, setTraceInput] = useState(params.traceId ?? '');
  const [data, setData] = useState<TraceResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    if (!params.traceId) {
      setData(null);
      return;
    }
    setLoading(true);
    setError('');
    api<TraceResponse>(`/api/admin/trace/${encodeURIComponent(params.traceId)}`)
      .then(setData)
      .catch((err) => setError(err instanceof Error ? err.message : 'Failed to load trace'))
      .finally(() => setLoading(false));
  }, [params.traceId]);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const id = traceInput.trim();
    if (!id) return;
    void navigate({ to: '/admin/trace/$traceId', params: { traceId: id } });
  };

  // Render timestamps in the user's locale so a second of clock skew
  // is easy to read, but keep full millisecond precision.
  const fmtTime = (iso: string) => {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return `${d.toLocaleTimeString(i18n.language, { hour12: false })}.${String(d.getMilliseconds()).padStart(3, '0')}`;
  };


  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('trace.title')}</h1>
        <p className="text-muted-foreground">{t('trace.subtitle')}</p>
      </div>

      <form onSubmit={handleSubmit} className="flex items-center gap-2">
        <Input
          value={traceInput}
          onChange={(e) => setTraceInput(e.target.value)}
          placeholder={t('trace.inputPlaceholder')}
          className="max-w-md font-mono text-xs"
          aria-label={t('trace.inputPlaceholder')}
        />
        <Button type="submit" disabled={!traceInput.trim()}>
          <Search className="mr-1.5 h-4 w-4" />
          {t('trace.lookup')}
        </Button>
      </form>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {loading && <Skeleton className="h-48 w-full" />}

      {!loading && data && (
        <Card>
          <CardHeader>
            <CardTitle className="font-mono text-sm">{data.trace_id}</CardTitle>
            <CardDescription>
              {t('trace.eventCount', { count: data.events.length })}
            </CardDescription>
          </CardHeader>
          <CardContent>
            {data.events.length === 0 ? (
              <p className="py-8 text-center text-muted-foreground">{t('trace.noEvents')}</p>
            ) : (
              <Waterfall events={data.events} fmtTime={fmtTime} />
            )}
          </CardContent>
        </Card>
      )}

      {!loading && !data && !params.traceId && (
        <Alert>
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{t('trace.howToFind')}</AlertDescription>
        </Alert>
      )}
    </div>
  );
}

/// SVG waterfall: each event is a rounded rectangle whose left edge is
/// the offset from the trace start and whose width is its duration.
/// Audit / instantaneous events get a minimum width so they're still
/// visible. Bars are colored by kind. Hovering a bar shows a tooltip
/// with the full subject + status. The whole thing is plain SVG —
/// adding a charting library for one viz would be overkill.
function Waterfall({
  events,
  fmtTime,
}: {
  events: TraceEvent[];
  fmtTime: (iso: string) => string;
}) {
  // Compute the time domain: trace start = earliest event,
  // trace end   = latest (event start + duration).
  const starts = events.map((e) => new Date(e.created_at).getTime()).filter(Number.isFinite);
  if (starts.length === 0) {
    return null;
  }
  const tMin = Math.min(...starts);
  const tMax = Math.max(
    ...events.map((e) => {
      const s = new Date(e.created_at).getTime();
      return Number.isFinite(s) ? s + Math.max(e.duration_ms, 0) : 0;
    }),
  );
  // Avoid divide-by-zero when every event lands on the same millisecond.
  const span = Math.max(1, tMax - tMin);

  // Layout constants. Geometry is fixed in SVG userspace; the outer
  // <svg> uses preserveAspectRatio="none" via viewBox so bars stretch
  // horizontally with the container while text stays a sane size.
  const ROW_H = 26;
  const LABEL_W = 200;
  const RIGHT_GUTTER = 60;
  const AXIS_H = 18;
  const PAD_TOP = 6;
  const PAD_BOTTOM = 6;
  const TRACK_W = 800; // viewBox-relative; CSS scales to container width
  const BAR_AREA_W = TRACK_W - LABEL_W - RIGHT_GUTTER;
  const MIN_BAR_W = 4;

  const totalH = PAD_TOP + AXIS_H + events.length * ROW_H + PAD_BOTTOM;

  const xForOffset = (offsetMs: number) => LABEL_W + (offsetMs / span) * BAR_AREA_W;

  // Color per (kind, status). Errors override the kind palette so
  // operators can spot a 502 / 5xx in a long timeline at a glance
  // without reading every tooltip.
  const isErrorEvent = (evt: TraceEvent): boolean => {
    if (evt.kind === 'gateway' || evt.kind === 'mcp') {
      const code = parseInt(evt.status, 10);
      // gateway uses HTTP codes (>= 400 is error). mcp emits "ok" /
      // "error" string statuses; both flow through here.
      return (
        (Number.isFinite(code) && code >= 400) ||
        evt.status === 'error'
      );
    }
    return false;
  };

  const colorFor = (evt: TraceEvent) => {
    if (isErrorEvent(evt)) {
      return 'var(--destructive)';
    }
    switch (evt.kind) {
      case 'gateway':
        return 'var(--chart-1)';
      case 'mcp':
        return 'var(--chart-2)';
      case 'audit':
        return 'var(--chart-3)';
      default:
        return 'var(--muted-foreground)';
    }
  };

  // Axis ticks at 0%, 25%, 50%, 75%, 100% of the span.
  const ticks = [0, 0.25, 0.5, 0.75, 1].map((p) => ({
    pct: p,
    x: LABEL_W + p * BAR_AREA_W,
    label: `${Math.round(p * span)}ms`,
  }));

  return (
    <div className="space-y-3">
      <svg
        viewBox={`0 0 ${TRACK_W} ${totalH}`}
        preserveAspectRatio="none"
        className="h-auto w-full font-mono"
        role="img"
        aria-label="Request trace waterfall"
      >
        {/* Axis line + ticks */}
        <line
          x1={LABEL_W}
          y1={PAD_TOP + AXIS_H}
          x2={LABEL_W + BAR_AREA_W}
          y2={PAD_TOP + AXIS_H}
          stroke="currentColor"
          strokeOpacity={0.2}
        />
        {ticks.map((t) => (
          <g key={t.pct}>
            <line
              x1={t.x}
              y1={PAD_TOP + AXIS_H - 4}
              x2={t.x}
              y2={PAD_TOP + AXIS_H + events.length * ROW_H}
              stroke="currentColor"
              strokeOpacity={t.pct === 0 || t.pct === 1 ? 0.2 : 0.08}
            />
            <text
              x={t.x}
              y={PAD_TOP + AXIS_H - 6}
              textAnchor={t.pct === 0 ? 'start' : t.pct === 1 ? 'end' : 'middle'}
              fontSize="10"
              fill="currentColor"
              fillOpacity={0.6}
            >
              {t.label}
            </text>
          </g>
        ))}

        {/* Rows */}
        {events.map((evt, i) => {
          const evtMs = new Date(evt.created_at).getTime();
          const offset = Number.isFinite(evtMs) ? evtMs - tMin : 0;
          const x = xForOffset(offset);
          const widthMs = Math.max(evt.duration_ms, 0);
          const widthPx = Math.max(MIN_BAR_W, (widthMs / span) * BAR_AREA_W);
          const y = PAD_TOP + AXIS_H + i * ROW_H + 4;
          const barH = ROW_H - 8;
          const labelText = evt.subject || '—';
          const tooltip =
            `[${evt.kind}] ${labelText}` +
            (evt.status ? ` · ${evt.status}` : '') +
            (evt.duration_ms > 0 ? ` · ${evt.duration_ms}ms` : '') +
            ` · ${fmtTime(evt.created_at)}`;
          return (
            <g key={`${evt.kind}-${evt.id}-${i}`}>
              <title>{tooltip}</title>
              {/* Left label: kind + subject (truncated). Use SVG text +
                  manual ellipsis-by-truncate; CSS overflow wouldn't
                  apply inside <text>. */}
              <text
                x={LABEL_W - 8}
                y={y + barH / 2 + 3}
                textAnchor="end"
                fontSize="10"
                fill="currentColor"
                fillOpacity={0.75}
              >
                {`${evt.kind}: ${labelText}`.slice(0, 28)}
                {`${evt.kind}: ${labelText}`.length > 28 ? '…' : ''}
              </text>
              <rect
                x={x}
                y={y}
                width={widthPx}
                height={barH}
                rx={3}
                fill={colorFor(evt)}
                fillOpacity={0.85}
              />
              {/* Duration label to the right of the bar when there's
                  room; suppressed for sub-ms point events. */}
              {evt.duration_ms > 0 && (
                <text
                  x={x + widthPx + 4}
                  y={y + barH / 2 + 3}
                  fontSize="10"
                  fill="currentColor"
                  fillOpacity={0.7}
                >
                  {evt.duration_ms}ms
                </text>
              )}
            </g>
          );
        })}
      </svg>

      {/* Legend */}
      <div className="flex items-center gap-4 text-xs text-muted-foreground">
        <span className="flex items-center gap-1.5">
          <span
            className="inline-block h-2 w-3 rounded-sm"
            style={{ background: 'var(--chart-1)' }}
            aria-hidden
          />
          gateway
        </span>
        <span className="flex items-center gap-1.5">
          <span
            className="inline-block h-2 w-3 rounded-sm"
            style={{ background: 'var(--chart-2)' }}
            aria-hidden
          />
          mcp
        </span>
        <span className="flex items-center gap-1.5">
          <span
            className="inline-block h-2 w-3 rounded-sm"
            style={{ background: 'var(--chart-3)' }}
            aria-hidden
          />
          audit
        </span>
        <span className="flex items-center gap-1.5">
          <span
            className="inline-block h-2 w-3 rounded-sm"
            style={{ background: 'var(--destructive)' }}
            aria-hidden
          />
          error
        </span>
      </div>
    </div>
  );
}
