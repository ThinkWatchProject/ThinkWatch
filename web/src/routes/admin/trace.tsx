import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useParams } from '@tanstack/react-router';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
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

  const kindBadgeVariant = (k: string): 'default' | 'secondary' | 'destructive' | 'outline' => {
    switch (k) {
      case 'gateway':
        return 'default';
      case 'mcp':
        return 'secondary';
      case 'audit':
        return 'outline';
      default:
        return 'outline';
    }
  };

  // Render timestamps in the user's locale so a second of clock skew
  // is easy to read, but keep full millisecond precision.
  const fmtTime = (iso: string) => {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return `${d.toLocaleTimeString(i18n.language, { hour12: false })}.${String(d.getMilliseconds()).padStart(3, '0')}`;
  };

  // Compute relative offset (ms) from the first event, so the waterfall
  // column reads like "0ms, +12ms, +410ms, ...".
  const baseMs = data && data.events.length > 0 ? new Date(data.events[0].created_at).getTime() : 0;

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
              <div className="space-y-1.5 font-mono text-xs">
                {data.events.map((evt, idx) => {
                  const evtMs = new Date(evt.created_at).getTime();
                  const offsetMs = Number.isFinite(evtMs) ? evtMs - baseMs : 0;
                  return (
                    <div
                      key={`${evt.kind}-${evt.id}-${idx}`}
                      className="grid grid-cols-[80px_70px_80px_1fr_80px_60px] items-center gap-2 rounded border bg-muted/30 px-2 py-1"
                    >
                      <span className="text-muted-foreground">{fmtTime(evt.created_at)}</span>
                      <span className="text-muted-foreground">
                        {idx === 0 ? '0ms' : `+${offsetMs}ms`}
                      </span>
                      <Badge variant={kindBadgeVariant(evt.kind)} className="justify-center">
                        {evt.kind}
                      </Badge>
                      <span className="truncate text-foreground">{evt.subject || '—'}</span>
                      <span className="text-right text-muted-foreground">{evt.status || '—'}</span>
                      <span className="text-right text-muted-foreground">
                        {evt.duration_ms > 0 ? `${evt.duration_ms}ms` : ''}
                      </span>
                    </div>
                  );
                })}
              </div>
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
