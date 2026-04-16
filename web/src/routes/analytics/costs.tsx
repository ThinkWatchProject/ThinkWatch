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
  /** Legacy single-dimension key */
  group_key?: string;
  /** New multi-dimension response shape */
  dimensions?: Record<string, string>;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  total_cost: string;
}

type CostDimension = 'model' | 'user' | 'cost_center' | 'provider';
const DIMENSION_OPTIONS: readonly CostDimension[] = ['model', 'user', 'cost_center', 'provider'] as const;
const MAX_DIMENSIONS = 2;

type TimeRange = '24h' | '7d' | '30d' | 'mtd';
const TIME_RANGE_OPTIONS: readonly TimeRange[] = ['24h', '7d', '30d', 'mtd'] as const;

interface CostStats {
  total_cost_mtd: number;
  budget_usage_pct: number | null;
}

/** Extract the display value for a dimension from a CostRow, handling both old and new API shapes. */
function getDimensionValue(row: CostRow, dim: CostDimension): string {
  if (row.dimensions && dim in row.dimensions) return row.dimensions[dim];
  // Legacy: group_key is only usable for single-dimension queries
  return row.group_key ?? '—';
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
  const [selectedDimensions, setSelectedDimensions] = useState<CostDimension[]>(['model']);
  const [timeRange, setTimeRange] = useState<TimeRange>('mtd');
  const [exporting, setExporting] = useState(false);

  const isSingleDimension = selectedDimensions.length === 1;

  const toggleDimension = useCallback((dim: CostDimension) => {
    setSelectedDimensions((prev) => {
      if (prev.includes(dim)) {
        // Don't allow deselecting the last dimension
        if (prev.length === 1) return prev;
        return prev.filter((d) => d !== dim);
      }
      if (prev.length >= MAX_DIMENSIONS) {
        // Replace the second dimension
        return [prev[0], dim];
      }
      return [...prev, dim];
    });
  }, []);

  const queryString = useCallback(
    (extra?: Record<string, string>): string => {
      const params = new URLSearchParams();
      if (selectedTeam) params.set('team_id', selectedTeam);
      params.set('group_by', selectedDimensions.join(','));
      if (timeRange !== 'mtd') params.set('range', timeRange);
      if (extra) for (const [k, v] of Object.entries(extra)) params.set(k, v);
      const qs = params.toString();
      return qs ? `?${qs}` : '';
    },
    [selectedTeam, selectedDimensions, timeRange],
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
      const res = await fetch(`/api/analytics/costs${queryString({ format: 'csv', limit: '1000' })}`, {
        credentials: 'include',
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `costs-${selectedDimensions.join('-')}-${new Date().toISOString().slice(0, 10)}.csv`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      toast.error(t('analyticsCosts.exportFailed', { msg: err instanceof Error ? err.message : 'unknown' }));
    } finally {
      setExporting(false);
    }
  }, [queryString, selectedDimensions, t]);

  // Chart: top N rows by cost for single-dimension mode
  const chartData = useMemo(() => {
    if (!isSingleDimension) return [];
    const dim = selectedDimensions[0];
    return rows
      .slice()
      .sort((a, b) => parseFloat(b.total_cost) - parseFloat(a.total_cost))
      .map((row) => {
        const label = getDimensionValue(row, dim);
        return {
          label: label.length > 18 ? label.slice(0, 16) + '..' : label,
          value: parseFloat(row.total_cost),
        };
      });
  }, [rows, isSingleDimension, selectedDimensions]);

  const totalCost = useMemo(() => rows.reduce((sum, r) => sum + parseFloat(r.total_cost), 0), [rows]);
  const budgetPct = stats.budget_usage_pct ?? 0;

  // ---- Pivot table data (2-dimension mode) ----
  const pivotData = useMemo(() => {
    if (isSingleDimension || selectedDimensions.length < 2) return null;

    const [rowDim, colDim] = selectedDimensions;
    const rowKeysSet = new Set<string>();
    const colKeysSet = new Set<string>();
    const cellMap = new Map<string, number>();

    for (const row of rows) {
      const rk = getDimensionValue(row, rowDim);
      const ck = getDimensionValue(row, colDim);
      rowKeysSet.add(rk);
      colKeysSet.add(ck);
      const key = `${rk}\0${ck}`;
      cellMap.set(key, (cellMap.get(key) ?? 0) + parseFloat(row.total_cost));
    }

    const rowKeys = [...rowKeysSet].sort();
    const colKeys = [...colKeysSet].sort();

    // Row totals
    const rowTotals = new Map<string, number>();
    for (const rk of rowKeys) {
      let sum = 0;
      for (const ck of colKeys) sum += cellMap.get(`${rk}\0${ck}`) ?? 0;
      rowTotals.set(rk, sum);
    }

    // Column totals
    const colTotals = new Map<string, number>();
    for (const ck of colKeys) {
      let sum = 0;
      for (const rk of rowKeys) sum += cellMap.get(`${rk}\0${ck}`) ?? 0;
      colTotals.set(ck, sum);
    }

    const grandTotal = [...rowTotals.values()].reduce((a, b) => a + b, 0);

    return { rowDim, colDim, rowKeys, colKeys, cellMap, rowTotals, colTotals, grandTotal };
  }, [rows, isSingleDimension, selectedDimensions]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('analyticsCosts.title')}</h1>
          <p className="text-muted-foreground">{t('analyticsCosts.subtitle')}</p>
        </div>
        <div className="flex items-center gap-2">
          {/* Time range selector */}
          <div
            role="radiogroup"
            aria-label={t('analyticsCosts.rangeLabel')}
            className="inline-flex items-center gap-0.5 rounded-md border bg-muted/30 p-0.5 text-xs"
          >
            {TIME_RANGE_OPTIONS.map((r) => (
              <button
                key={r}
                type="button"
                role="radio"
                aria-checked={timeRange === r}
                onClick={() => setTimeRange(r)}
                className={`rounded px-2 py-1 font-medium transition-colors ${
                  timeRange === r
                    ? 'bg-background text-foreground shadow-sm'
                    : 'text-muted-foreground hover:text-foreground'
                }`}
              >
                {t(`analyticsCosts.range_${r}`)}
              </button>
            ))}
          </div>

          {/* Dimension toggles (multi-select, max 2) */}
          <div
            role="group"
            aria-label={t('analyticsCosts.groupBy')}
            className="inline-flex items-center gap-0.5 rounded-md border bg-muted/30 p-0.5 text-xs"
          >
            {DIMENSION_OPTIONS.map((g) => (
              <button
                key={g}
                type="button"
                role="checkbox"
                aria-checked={selectedDimensions.includes(g)}
                onClick={() => toggleDimension(g)}
                className={`rounded px-2 py-1 font-medium transition-colors ${
                  selectedDimensions.includes(g)
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

      {/* Bar chart -- only shown for single-dimension mode */}
      {isSingleDimension && (
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
      )}

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
          ) : isSingleDimension ? (
            /* ---------- Single-dimension: original flat table ---------- */
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t(`analyticsCosts.group.${selectedDimensions[0]}`)}</TableHead>
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
                  const label = getDimensionValue(row, selectedDimensions[0]);
                  return (
                    <TableRow key={label}>
                      <TableCell className="font-mono text-xs">{label}</TableCell>
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
          ) : pivotData ? (
            /* ---------- Two-dimension: pivot table ---------- */
            <div className="overflow-x-auto">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>
                      {t(`analyticsCosts.group.${pivotData.rowDim}`)} / {t(`analyticsCosts.group.${pivotData.colDim}`)}
                    </TableHead>
                    {pivotData.colKeys.map((ck) => (
                      <TableHead key={ck} className="text-right font-mono text-xs">
                        {ck}
                      </TableHead>
                    ))}
                    <TableHead className="text-right font-semibold">{t('analyticsCosts.pivotTotal')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {pivotData.rowKeys.map((rk) => (
                    <TableRow key={rk}>
                      <TableCell className="font-mono text-xs">{rk}</TableCell>
                      {pivotData.colKeys.map((ck) => {
                        const val = pivotData.cellMap.get(`${rk}\0${ck}`) ?? 0;
                        return (
                          <TableCell key={ck} className="text-right tabular-nums">
                            {val > 0 ? `$${val.toFixed(2)}` : '—'}
                          </TableCell>
                        );
                      })}
                      <TableCell className="text-right font-semibold tabular-nums">
                        ${(pivotData.rowTotals.get(rk) ?? 0).toFixed(2)}
                      </TableCell>
                    </TableRow>
                  ))}
                  {/* Column totals row */}
                  <TableRow className="border-t-2">
                    <TableCell className="font-semibold">{t('analyticsCosts.pivotTotal')}</TableCell>
                    {pivotData.colKeys.map((ck) => (
                      <TableCell key={ck} className="text-right font-semibold tabular-nums">
                        ${(pivotData.colTotals.get(ck) ?? 0).toFixed(2)}
                      </TableCell>
                    ))}
                    <TableCell className="text-right font-bold tabular-nums">
                      ${pivotData.grandTotal.toFixed(2)}
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </div>
          ) : null}
        </CardContent>
      </Card>
    </div>
  );
}
