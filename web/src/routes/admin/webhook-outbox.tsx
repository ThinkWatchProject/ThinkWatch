import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Checkbox } from '@/components/ui/checkbox';
import { AlertCircle, RefreshCw, Trash2, Inbox } from 'lucide-react';
import { api, apiDelete, apiPost } from '@/lib/api';
import { toast } from 'sonner';

interface OutboxRow {
  id: string;
  forwarder_id: string;
  forwarder_name: string | null;
  forwarder_url: string | null;
  attempts: number;
  next_attempt_at: string;
  last_error: string | null;
  created_at: string;
}

interface OutboxResponse {
  items: OutboxRow[];
  total: number;
}

/// Admin view for the durable webhook retry queue. The drain worker
/// runs every 10s automatically; this page exists for operators who
/// need to *see* what's stuck (capacity / dead-receiver triage) and
/// occasionally prod a single row (force-retry after fixing the
/// receiver, or drop after confirming the receiver is gone).
export function WebhookOutboxPage() {
  const { t, i18n } = useTranslation();
  const [data, setData] = useState<OutboxResponse | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  // Auto-refresh keeps the queue view current while the drain worker
  // chews through the backlog. 10s matches the worker cadence — a
  // faster poll just wastes CH / PG round-trips. Default on so the
  // operator watching a drain-down doesn't have to hand-refresh.
  const [autoRefresh, setAutoRefresh] = useState(true);

  const load = async (isInitial: boolean) => {
    if (isInitial) setLoading(true);
    setError('');
    try {
      const res = await api<OutboxResponse>('/api/admin/webhook-outbox');
      setData(res);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load');
    } finally {
      if (isInitial) setLoading(false);
    }
  };

  useEffect(() => {
    void load(true);
  }, []);

  useEffect(() => {
    if (!autoRefresh) return;
    const id = window.setInterval(() => {
      void load(false);
    }, 10_000);
    return () => window.clearInterval(id);
  }, [autoRefresh]);

  const handleRetry = async (id: string) => {
    setBusyId(id);
    try {
      await apiPost(`/api/admin/webhook-outbox/${id}/retry`, {});
      toast.success(t('webhookOutbox.retryQueued'));
      await load(false);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Retry failed');
    } finally {
      setBusyId(null);
    }
  };

  const handleDelete = async (id: string) => {
    if (!window.confirm(t('webhookOutbox.deleteConfirm'))) return;
    setBusyId(id);
    try {
      await apiDelete(`/api/admin/webhook-outbox/${id}`);
      toast.success(t('webhookOutbox.deleted'));
      await load(false);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Delete failed');
    } finally {
      setBusyId(null);
    }
  };

  const fmtTime = (iso: string) => {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(i18n.language);
  };

  // "next attempt" relative-time hint — operators care more about
  // "due in 12s" than wall-clock when the queue is moving.
  const fmtRelative = (iso: string) => {
    const d = new Date(iso).getTime();
    if (!Number.isFinite(d)) return '';
    const delta = Math.round((d - Date.now()) / 1000);
    if (delta <= 0) return t('webhookOutbox.due');
    if (delta < 60) return `${delta}s`;
    if (delta < 3600) return `${Math.round(delta / 60)}m`;
    return `${Math.round(delta / 3600)}h`;
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('webhookOutbox.title')}</h1>
          <p className="text-muted-foreground">{t('webhookOutbox.subtitle')}</p>
        </div>
        <div className="flex items-center gap-3">
          <label className="flex cursor-pointer items-center gap-1.5 text-xs text-muted-foreground">
            <Checkbox
              checked={autoRefresh}
              onCheckedChange={(v) => setAutoRefresh(v === true)}
            />
            {t('webhookOutbox.autoRefresh')}
          </label>
          <Button variant="outline" size="sm" onClick={() => load(false)} disabled={loading}>
            <RefreshCw className={`mr-1 h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
            {t('common.refresh')}
          </Button>
        </div>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('webhookOutbox.pending')}</CardTitle>
          {data && (
            <CardDescription>
              {data.items.length === data.total
                ? t('webhookOutbox.totalRows', { count: data.total })
                : t('webhookOutbox.totalRowsTruncated', {
                    shown: data.items.length,
                    total: data.total,
                  })}
            </CardDescription>
          )}
        </CardHeader>
        <CardContent>
          {loading && !data ? (
            <Skeleton className="h-32 w-full" />
          ) : !data || data.items.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-10 text-center text-muted-foreground">
              <Inbox className="mb-2 h-10 w-10" />
              <p className="text-sm">{t('webhookOutbox.empty')}</p>
            </div>
          ) : (
            <div className="space-y-2">
              {data.items.map((r) => (
                <div
                  key={r.id}
                  className="grid grid-cols-[1fr_120px_120px_70px_auto] items-center gap-3 rounded border bg-muted/30 p-3 text-sm"
                >
                  <div className="min-w-0 space-y-1">
                    <div className="font-mono text-xs">
                      {r.forwarder_name ?? <span className="italic text-muted-foreground">{t('webhookOutbox.deletedForwarder')}</span>}
                    </div>
                    {r.forwarder_url && (
                      <div
                        className="truncate font-mono text-[10px] text-muted-foreground"
                        title={r.forwarder_url}
                      >
                        {r.forwarder_url}
                      </div>
                    )}
                    {r.last_error && (
                      <div
                        className="truncate font-mono text-[10px] text-destructive"
                        title={r.last_error}
                      >
                        {r.last_error}
                      </div>
                    )}
                  </div>
                  <div className="text-xs text-muted-foreground">
                    <div>{t('webhookOutbox.created')}</div>
                    <div className="font-mono">{fmtTime(r.created_at)}</div>
                  </div>
                  <div className="text-xs text-muted-foreground">
                    <div>{t('webhookOutbox.nextAttempt')}</div>
                    <div className="font-mono" title={fmtTime(r.next_attempt_at)}>
                      {fmtRelative(r.next_attempt_at)}
                    </div>
                  </div>
                  <div className="text-center">
                    <Badge
                      variant={r.attempts >= 18 ? 'destructive' : r.attempts >= 6 ? 'default' : 'secondary'}
                      className="font-mono"
                      title={t('webhookOutbox.attemptsHint')}
                    >
                      {r.attempts}/24
                    </Badge>
                  </div>
                  <div className="flex gap-1">
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7"
                      disabled={busyId === r.id}
                      onClick={() => handleRetry(r.id)}
                      aria-label={t('webhookOutbox.retry')}
                      title={t('webhookOutbox.retry')}
                    >
                      <RefreshCw className="h-3.5 w-3.5" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7 text-destructive"
                      disabled={busyId === r.id}
                      onClick={() => handleDelete(r.id)}
                      aria-label={t('webhookOutbox.delete')}
                      title={t('webhookOutbox.delete')}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      <Alert>
        <AlertCircle className="h-4 w-4" />
        <AlertDescription>{t('webhookOutbox.workerHint')}</AlertDescription>
      </Alert>
    </div>
  );
}
