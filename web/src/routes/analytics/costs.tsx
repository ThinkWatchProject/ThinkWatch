import { useCallback, useEffect, useState, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { DollarSign, TrendingUp, AlertCircle, Download } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { api } from '@/lib/api';
import { toast } from 'sonner';
import { useTeams } from '@/hooks/use-teams';
import { TeamFilter } from '@/components/filters/team-filter';
import { SimpleBarChart } from '@/components/ui/simple-chart';
import { Skeleton } from '@/components/ui/skeleton';
import { Progress } from '@/components/ui/progress';

interface CostRow {
  group_key: string;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  total_cost: string;
}

type CostGroupBy = 'model' | 'user' | 'team' | 'cost_center';
const GROUP_BY_OPTIONS: readonly CostGroupBy[] = ['model', 'user', 'team', 'cost_center'] as const;

interface CostStats {
  total_cost_mtd: number;
  budget_usage_pct: number | null;
}

export function CostsPage() {
  const { t } = useTranslation();
  const [rows, setRows] = useState<CostRow[]>([]);
  const [stats, setStats] = useState<CostStats>({ total_cost_mtd: 0, budget_usage_pct: null });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  // Team filter
  const { teams } = useTeams();
  const [selectedTeam, setSelectedTeam] = useState<string>('');
  const [groupBy, setGroupBy] = useState<CostGroupBy>('model');
  const [exporting, setExporting] = useState(false);

  const queryString = useCallback(
    (extra?: Record<string, string>): string => {
      const params = new URLSearchParams();
      if (selectedTeam) params.set('team_id', selectedTeam);
      params.set('group_by', groupBy);
      if (extra) for (const [k, v] of Object.entries(extra)) params.set(k, v);
      const qs = params.toString();
      return qs ? `?${qs}` : '';
    },
    [selectedTeam, groupBy],
  );

  const fetchData = useCallback(() => {
    setLoading(true);
    Promise.all([
      api<CostRow[]>(`/api/analytics/costs${queryString()}`),
      api<CostStats>(`/api/analytics/costs/stats${selectedTeam ? `?team_id=${selectedTeam}` : ''}`),
    ])
      .then(([costData, statsData]) => {
        setRows(costData);
        setStats(statsData);
      })
      .catch((err) => setError(err instanceof Error ? err.message : 'Failed to load cost data'))
      .finally(() => setLoading(false));
  }, [queryString, selectedTeam]);

  useEffect(() => {
    fetchData();
  }, [fetchData]);

  const handleExport = useCallback(async () => {
    setExporting(true);
    try {
      // Use fetch directly — the /costs endpoint returns text/csv with
      // `format=csv`, which the typed api() helper would try to JSON-parse.
      const res = await fetch(`/api/analytics/costs${queryString({ format: 'csv', limit: '1000' })}`, {
        credentials: 'include',
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `costs-${groupBy}-${new Date().toISOString().slice(0, 10)}.csv`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      toast.error(t('analyticsCosts.exportFailed', { msg: err instanceof Error ? err.message : 'unknown' }));
    } finally {
      setExporting(false);
    }
  }, [queryString, groupBy, t]);

  // Chart: top N rows by cost for the selected grouping.
  const chartData = useMemo(() => {
    return rows
      .slice()
      .sort((a, b) => parseFloat(b.total_cost) - parseFloat(a.total_cost))
      .map((row) => ({
        label: row.group_key.length > 18 ? row.group_key.slice(0, 16) + '..' : row.group_key,
        value: parseFloat(row.total_cost),
      }));
  }, [rows]);

  const totalCost = useMemo(() => rows.reduce((sum, r) => sum + parseFloat(r.total_cost), 0), [rows]);
  const budgetPct = stats.budget_usage_pct ?? 0;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('analyticsCosts.title')}</h1>
          <p className="text-muted-foreground">{t('analyticsCosts.subtitle')}</p>
        </div>
        <div className="flex items-center gap-2">
          <div
            role="radiogroup"
            aria-label={t('analyticsCosts.groupBy')}
            className="inline-flex items-center gap-0.5 rounded-md border bg-muted/30 p-0.5 text-xs"
          >
            {GROUP_BY_OPTIONS.map((g) => (
              <button
                key={g}
                type="button"
                role="radio"
                aria-checked={groupBy === g}
                onClick={() => setGroupBy(g)}
                className={`rounded px-2 py-1 font-medium transition-colors ${
                  groupBy === g
                    ? 'bg-background text-foreground shadow-sm'
                    : 'text-muted-foreground hover:text-foreground'
                }`}
              >
                {t(`analyticsCosts.group.${g}`)}
              </button>
            ))}
          </div>
          <Button variant="outline" size="sm" disabled={exporting || rows.length === 0} onClick={handleExport}>
            <Download className="mr-1.5 h-3.5 w-3.5" />
            {t('analyticsCosts.export')}
          </Button>
          <TeamFilter teams={teams} value={selectedTeam} onChange={setSelectedTeam} />
        </div>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('analyticsCosts.totalCostMtd')}</CardTitle>
            <DollarSign className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {loading ? <Skeleton className="h-8 w-24" /> : `$${stats.total_cost_mtd.toFixed(2)}`}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('analyticsCosts.budgetUsage')}</CardTitle>
            <TrendingUp className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {loading ? <Skeleton className="h-8 w-24" /> : stats.budget_usage_pct != null ? `${budgetPct.toFixed(1)}%` : '—'}
            </div>
            {stats.budget_usage_pct != null && (
              <Progress value={Math.min(budgetPct, 100)} className="mt-2" />
            )}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('analyticsCosts.costByModel')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <Skeleton className="h-48 w-full" />
          ) : chartData.length === 0 ? (
            <div className="flex h-48 items-center justify-center text-muted-foreground">{t('analyticsCosts.noCosts')}</div>
          ) : (
            <SimpleBarChart data={chartData} formatValue={(v) => `$${v.toFixed(4)}`} />
          )}
        </CardContent>
      </Card>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('analyticsCosts.costTrend')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(4)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-36" />
                  <Skeleton className="h-4 w-16" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : rows.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <DollarSign className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('analyticsCosts.noCosts')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t(`analyticsCosts.group.${groupBy}`)}</TableHead>
                  <TableHead className="text-right">{t('analyticsCosts.requests')}</TableHead>
                  <TableHead className="text-right">{t('analyticsCosts.inputTokens')}</TableHead>
                  <TableHead className="text-right">{t('analyticsCosts.outputTokens')}</TableHead>
                  <TableHead className="text-right">{t('analyticsCosts.totalCost')}</TableHead>
                  <TableHead className="text-right">{t('analyticsCosts.percentOfTotal')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((row) => {
                  const cost = parseFloat(row.total_cost);
                  const pct = totalCost > 0 ? (cost / totalCost) * 100 : 0;
                  return (
                    <TableRow key={row.group_key}>
                      <TableCell className="font-mono text-xs">{row.group_key}</TableCell>
                      <TableCell className="text-right">{row.request_count.toLocaleString()}</TableCell>
                      <TableCell className="text-right">{row.input_tokens.toLocaleString()}</TableCell>
                      <TableCell className="text-right">{row.output_tokens.toLocaleString()}</TableCell>
                      <TableCell className="text-right">${cost.toFixed(4)}</TableCell>
                      <TableCell className="text-right">{pct.toFixed(1)}%</TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
