import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertCircle, Inbox, RefreshCw, Trash2 } from 'lucide-react';
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

interface OutboxBacklogDialogProps {
  /// Forwarder id whose backlog to show. `null` / empty closes the dialog.
  forwarderId: string | null;
  /// Label shown in the header so the operator remembers which
  /// destination they're triaging. Passed in by the list page that
  /// already has it loaded.
  forwarderName?: string;
  onOpenChange: (open: boolean) => void;
  /// Called after a retry / delete / natural drain so the parent can
  /// refresh its backlog counts column without a separate poll.
  onChanged?: () => void;
}

/// Per-forwarder backlog triage — the content that used to live on the
/// standalone `/admin/webhook-outbox` page. Scoped to a single
/// forwarder via `?forwarder_id=` so operators open it directly from
/// the log-forwarders row that's backing up.
///
/// Auto-refreshes every 10s (matches the drain worker cadence) so a
/// live drain-down is visible without hand-refreshing.
export function OutboxBacklogDialog({
  forwarderId,
  forwarderName,
  onOpenChange,
  onChanged,
}: OutboxBacklogDialogProps) {
  const { t, i18n } = useTranslation();
  const [data, setData] = useState<OutboxResponse | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);

  const load = useCallback(
    async (isInitial: boolean) => {
      if (!forwarderId) return;
      if (isInitial) setLoading(true);
      setError('');
      try {
        const res = await api<OutboxResponse>(
          `/api/admin/webhook-outbox?forwarder_id=${forwarderId}`,
        );
        setData(res);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load');
      } finally {
        if (isInitial) setLoading(false);
      }
    },
    [forwarderId],
  );

  useEffect(() => {
    if (!forwarderId) {
      setData(null);
      return;
    }
    void load(true);
  }, [forwarderId, load]);

  useEffect(() => {
    if (!forwarderId || !autoRefresh) return;
    const id = window.setInterval(() => void load(false), 10_000);
    return () => window.clearInterval(id);
  }, [forwarderId, autoRefresh, load]);

  const handleRetry = async (id: string) => {
    setBusyId(id);
    try {
      await apiPost(`/api/admin/webhook-outbox/${id}/retry`, {});
      toast.success(t('webhookOutbox.retryQueued'));
      await load(false);
      onChanged?.();
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
      onChanged?.();
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
    <Dialog open={!!forwarderId} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-3xl max-h-[85vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center justify-between gap-3">
            <span>
              {t('webhookOutbox.title')}
              {forwarderName && (
                <span className="ml-2 font-mono text-sm font-normal text-muted-foreground">
                  {forwarderName}
                </span>
              )}
            </span>
            <div className="flex items-center gap-3 text-xs font-normal">
              <label className="flex cursor-pointer items-center gap-1.5 text-muted-foreground">
                <Checkbox
                  checked={autoRefresh}
                  onCheckedChange={(v) => setAutoRefresh(v === true)}
                />
                {t('webhookOutbox.autoRefresh')}
              </label>
              <Button
                variant="outline"
                size="sm"
                onClick={() => load(false)}
                disabled={loading}
              >
                <RefreshCw
                  className={`mr-1 h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`}
                />
                {t('common.refresh')}
              </Button>
            </div>
          </DialogTitle>
          <DialogDescription>
            {t('webhookOutbox.subtitle')}
            {data && data.items.length > 0 && (
              <span className="ml-2 text-muted-foreground">
                {data.items.length === data.total
                  ? t('webhookOutbox.totalRows', { count: data.total })
                  : t('webhookOutbox.totalRowsTruncated', {
                      shown: data.items.length,
                      total: data.total,
                    })}
              </span>
            )}
          </DialogDescription>
        </DialogHeader>

        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

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
                className="grid grid-cols-[1fr_110px_90px_70px_auto] items-center gap-3 rounded border bg-muted/30 p-3 text-sm"
              >
                <div className="min-w-0 space-y-1">
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
                    variant={
                      r.attempts >= 18
                        ? 'destructive'
                        : r.attempts >= 6
                          ? 'default'
                          : 'secondary'
                    }
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

        <Alert className="mt-2">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{t('webhookOutbox.workerHint')}</AlertDescription>
        </Alert>
      </DialogContent>
    </Dialog>
  );
}
