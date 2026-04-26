import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Switch } from '@/components/ui/switch';
import { Label } from '@/components/ui/label';
import { Activity, RefreshCw } from 'lucide-react';
import { api } from '@/lib/api';
import { toast } from 'sonner';
import { format } from 'date-fns';

interface CandidateRecord {
  route_id: string;
  provider_name: string;
  upstream_model: string | null;
  weight: number;
  health_state: string;
  ewma_latency_ms?: number | null;
  excluded_reason?: string | null;
}

interface DecisionRecord {
  ts_ms: number;
  model_id: string;
  strategy: string;
  affinity_mode: string;
  affinity_hit: boolean;
  candidates: CandidateRecord[];
  picked_route_id?: string | null;
  attempts: number;
  total_latency_ms: number;
  success: boolean;
  error_message?: string | null;
  user_hint?: string | null;
}

const POLL_INTERVAL_MS = 5_000;

/// Live tail of routing decisions. Backed by Redis (24h TTL) and not
/// persisted — open the page when you need to reason about a recent
/// burst of traffic, expect the buffer to be small.
export function RouteDecisionsPage() {
  const { t } = useTranslation();
  const [decisions, setDecisions] = useState<DecisionRecord[]>([]);
  const [models, setModels] = useState<string[]>([]);
  const [filterModel, setFilterModel] = useState<string>('all');
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [loading, setLoading] = useState(false);

  const refresh = useMemo(
    () => async () => {
      setLoading(true);
      try {
        const [list, modelsRes] = await Promise.all([
          api<{ items: DecisionRecord[] }>(
            `/api/admin/route-decisions?limit=200${
              filterModel !== 'all'
                ? `&model_id=${encodeURIComponent(filterModel)}`
                : ''
            }`,
          ),
          api<{ items: string[] }>('/api/admin/route-decisions/models'),
        ]);
        setDecisions(list.items);
        setModels(modelsRes.items);
      } catch (err) {
        toast.error(err instanceof Error ? err.message : 'Failed to load decisions');
      } finally {
        setLoading(false);
      }
    },
    [filterModel],
  );

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => {
      void refresh();
    }, POLL_INTERVAL_MS);
    return () => clearInterval(id);
  }, [autoRefresh, refresh]);

  return (
    <div className="space-y-4 p-4">
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle className="flex items-center gap-2">
              <Activity className="h-5 w-5" />
              {t('routeDecisions.title')}
            </CardTitle>
            <p className="mt-1 text-xs text-muted-foreground">
              {t('routeDecisions.subtitle')}
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Select value={filterModel} onValueChange={setFilterModel}>
              <SelectTrigger className="w-64">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">{t('routeDecisions.allModels')}</SelectItem>
                {models.map((m) => (
                  <SelectItem key={m} value={m}>
                    {m}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <div className="flex items-center gap-2">
              <Switch
                id="auto-refresh"
                checked={autoRefresh}
                onCheckedChange={setAutoRefresh}
              />
              <Label htmlFor="auto-refresh" className="text-xs">
                {t('routeDecisions.autoRefresh')}
              </Label>
            </div>
            <Button
              variant="outline"
              size="sm"
              onClick={() => void refresh()}
              disabled={loading}
            >
              <RefreshCw
                className={loading ? 'h-4 w-4 animate-spin' : 'h-4 w-4'}
              />
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {decisions.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              {t('routeDecisions.empty')}
            </p>
          ) : (
            <ScrollArea className="h-[70vh] rounded-md border">
              <div className="divide-y">
                {decisions.map((d, i) => (
                  <DecisionRow key={`${d.ts_ms}-${i}`} d={d} />
                ))}
              </div>
            </ScrollArea>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function DecisionRow({ d }: { d: DecisionRecord }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-2 p-3 text-xs">
      <div className="flex items-center gap-2">
        <span className="font-mono text-muted-foreground">
          {format(new Date(d.ts_ms), 'HH:mm:ss.SSS')}
        </span>
        <span className="font-mono">{d.model_id}</span>
        <Badge variant={d.success ? 'default' : 'destructive'}>
          {d.success ? t('routeDecisions.ok') : t('routeDecisions.fail')}
        </Badge>
        <Badge variant="outline">{d.strategy}</Badge>
        <Badge variant="outline">
          {t('routeDecisions.affinityPrefix')}: {d.affinity_mode}
          {d.affinity_hit && (
            <span className="ml-1 text-green-500">{t('routeDecisions.hit')}</span>
          )}
        </Badge>
        {d.attempts > 1 && (
          <Badge variant="secondary">
            {t('routeDecisions.attempts', { count: d.attempts })}
          </Badge>
        )}
        <span className="ml-auto text-muted-foreground">
          {d.total_latency_ms} ms
        </span>
      </div>
      {d.error_message && (
        <div className="rounded bg-destructive/10 p-2 font-mono text-destructive">
          {d.error_message}
        </div>
      )}
      <div className="grid gap-1 pl-4">
        {d.candidates.map((c) => {
          const picked = c.route_id === d.picked_route_id;
          return (
            <div
              key={c.route_id}
              className={`flex items-center gap-2 ${
                picked ? 'font-medium' : 'text-muted-foreground'
              }`}
            >
              <span className="w-2 text-center">{picked ? '→' : ' '}</span>
              <span className="font-mono">{c.provider_name}</span>
              {c.upstream_model && (
                <span className="font-mono italic">{c.upstream_model}</span>
              )}
              <span className="ml-auto flex items-center gap-2">
                <span className="font-mono">w={c.weight.toFixed(3)}</span>
                {c.ewma_latency_ms != null && (
                  <span className="font-mono">
                    p50≈{c.ewma_latency_ms.toFixed(0)}ms
                  </span>
                )}
                <Badge
                  variant={
                    c.health_state === 'closed'
                      ? 'outline'
                      : c.health_state === 'half_open'
                        ? 'secondary'
                        : 'destructive'
                  }
                >
                  {c.health_state}
                </Badge>
                {c.excluded_reason && (
                  <Badge variant="destructive" className="text-xs">
                    {c.excluded_reason}
                  </Badge>
                )}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
