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
import { DollarSign, TrendingUp, TrendingDown, AlertCircle, Download, ChevronDown } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { api } from '@/lib/api';
import { toast } from 'sonner';
import { useTeams } from '@/hooks/use-teams';
import { TeamFilter } from '@/components/filters/team-filter';
import { SimpleBarChart } from '@/components/ui/simple-chart';
import { Skeleton } from '@/components/ui/skeleton';
import { Progress } from '@/components/ui/progress';
import { Badge } from '@/components/ui/badge';
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import Decimal from 'decimal.js';

interface CostRow {
  dimensions: Record<string, string>;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  // Decimal string on the wire (CH `Decimal(18, 10)`). Kept as a
  // string in state so every sort / sum / display goes through
  // decimal.js instead of round-tripping through f64.
  total_cost: string;
}

type CostDimension = 'model' | 'user' | 'cost_center' | 'provider';
const DIMENSION_OPTIONS: readonly CostDimension[] = ['model', 'user', 'cost_center', 'provider'] as const;
const MAX_DIMENSIONS = 2;

type TimeRange = '24h' | '7d' | '30d' | 'mtd';
const TIME_RANGE_OPTIONS: readonly TimeRange[] = ['24h', '7d', '30d', 'mtd'] as const;

interface CostStats {
  total_cost_mtd: string;
  budget_usage_pct: number | null;
  // Period-scoped totals from the same `range` the chart is showing.
  // Wire format is a Decimal string (rust_decimal::serde::str on the
  // server) so JS never sees an f64. Both fields are optional because
  // older payloads / non-compare requests may omit them.
  total_cost?: string;
  prev_total_cost?: string;
}

/** Extract the display value for a dimension from a CostRow. */
function getDimensionValue(row: CostRow, dim: CostDimension): string {
  return row.dimensions[dim] ?? '—';
}

export function CostsPage() {
  const { t } = useTranslation();
  const [rows, setRows] = useState<CostRow[]>([]);
  const [stats, setStats] = useState<CostStats>({ total_cost_mtd: '0', budget_usage_pct: null });
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

  const fetchData = useCallback((signal?: AbortSignal) => {
    setLoading(true);
    // Cost-stats endpoint accepts `range=24h|7d|30d` (anything else
    // — including `mtd` — falls back to 24h server-side). Forward the
    // page's range so the period total + delta match the chart, and
    // request `compare=true` to populate `prev_total_cost`.
    const statsParams = new URLSearchParams();
    if (selectedTeam) statsParams.set('team_id', selectedTeam);
    if (timeRange !== 'mtd') {
      statsParams.set('range', timeRange);
      statsParams.set('compare', 'true');
    }
    const statsQs = statsParams.toString();
    Promise.all([
      api<{ items: CostRow[]; total: { request_count: number; input_tokens: number; output_tokens: number; total_cost: string } }>(`/api/analytics/costs${queryString()}`, { signal }),
      api<CostStats>(`/api/analytics/costs/stats${statsQs ? `?${statsQs}` : ''}`, { signal }),
    ])
      .then(([costData, statsData]) => {
        if (signal?.aborted) return;
        setRows(costData.items);
        setStats(statsData);
      })
      .catch((err) => {
        // Swallow aborts — the next effect tick will refetch with
        // the new params. Surface real errors only.
        if (err instanceof DOMException && err.name === 'AbortError') return;
        if (signal?.aborted) return;
        setError(err instanceof Error ? err.message : t('common.error'));
      })
      .finally(() => {
        if (signal?.aborted) return;
        setLoading(false);
      });
  }, [queryString, selectedTeam, timeRange, t]);

  useEffect(() => {
    // Cancel the in-flight pair when params change OR the page
    // unmounts. Without this, rapid toggling (e.g. clicking time-
    // range chips fast) lets a stale Promise.all settle last and
    // overwrite the fresher data with older numbers.
    const controller = new AbortController();
    fetchData(controller.signal);
    return () => controller.abort();
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
      // Override the server's Content-Disposition filename with one
      // that captures the *selected dimensions* — the server only
      // knows the time window. curl users get the server fallback;
      // browser users get this richer name.
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

  // Chart: top N rows by cost for single-dimension mode. Sort via
  // Decimal so extreme values don't get compared as f64.
  const chartData = useMemo(() => {
    if (!isSingleDimension) return [];
    const dim = selectedDimensions[0];
    return rows
      .slice()
      .sort((a, b) => new Decimal(b.total_cost).comparedTo(new Decimal(a.total_cost)))
      .map((row) => {
        const label = getDimensionValue(row, dim);
        return {
          label: label.length > 18 ? label.slice(0, 16) + '..' : label,
          // simple-chart takes a number; converting to Number here
          // keeps the visual plot happy while the source of truth
          // stays as a Decimal string on every other path.
          value: new Decimal(row.total_cost).toNumber(),
        };
      });
  }, [rows, isSingleDimension, selectedDimensions]);

  const totalCost = useMemo(
    () =>
      rows.reduce(
        (sum, r) => sum.plus(new Decimal(r.total_cost)),
        new Decimal(0),
      ),
    [rows],
  );
  const budgetPct = stats.budget_usage_pct ?? 0;

  // Period-scoped total + delta vs. the previous equal-length window.
  // MTD has its own card already, so the period card only renders for
  // 24h / 7d / 30d. Everything stays in `Decimal` so the precision
  // story matches the rest of the page.
  const showPeriodCard = timeRange !== 'mtd';
  const periodTotal = useMemo(
    () =>
      stats.total_cost != null ? new Decimal(stats.total_cost) : null,
    [stats.total_cost],
  );
  const periodDeltaPct = useMemo(() => {
    if (!periodTotal || stats.prev_total_cost == null) return null;
    const prev = new Decimal(stats.prev_total_cost);
    // Avoid divide-by-zero; the hint is irrelevant when there was no
    // prior spend to compare against.
    if (prev.isZero()) return null;
    return periodTotal.minus(prev).dividedBy(prev).times(100);
  }, [periodTotal, stats.prev_total_cost]);

  // ---- Pivot table data (2-dimension mode) ----
  const pivotData = useMemo(() => {
    if (isSingleDimension || selectedDimensions.length < 2) return null;

    const [rowDim, colDim] = selectedDimensions;
    const rowKeysSet = new Set<string>();
    const colKeysSet = new Set<string>();
    const cellMap = new Map<string, Decimal>();

    for (const row of rows) {
      const rk = getDimensionValue(row, rowDim);
      const ck = getDimensionValue(row, colDim);
      rowKeysSet.add(rk);
      colKeysSet.add(ck);
      const key = `${rk}\0${ck}`;
      const prev = cellMap.get(key) ?? new Decimal(0);
      cellMap.set(key, prev.plus(new Decimal(row.total_cost)));
    }

    const rowKeys = [...rowKeysSet].sort();
    const colKeys = [...colKeysSet].sort();

    // Row totals
    const rowTotals = new Map<string, Decimal>();
    for (const rk of rowKeys) {
      let sum = new Decimal(0);
      for (const ck of colKeys)
        sum = sum.plus(cellMap.get(`${rk}\0${ck}`) ?? new Decimal(0));
      rowTotals.set(rk, sum);
    }

    // Column totals
    const colTotals = new Map<string, Decimal>();
    for (const ck of colKeys) {
      let sum = new Decimal(0);
      for (const rk of rowKeys)
        sum = sum.plus(cellMap.get(`${rk}\0${ck}`) ?? new Decimal(0));
      colTotals.set(ck, sum);
    }

    const grandTotal = [...rowTotals.values()].reduce(
      (a, b) => a.plus(b),
      new Decimal(0),
    );

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

          {/* Dimension picker — dropdown + badge chips */}
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="outline" size="sm" className="gap-1.5">
                {t('analyticsCosts.groupBy')}
                <span className="inline-flex items-center gap-1">
                  {selectedDimensions.map((dim, idx) => (
                    <Badge key={dim} variant="secondary" className="gap-0.5 text-[10px] leading-none">
                      {!isSingleDimension && (
                        <span className="text-muted-foreground">
                          {idx === 0 ? t('analyticsCosts.dimRow') : t('analyticsCosts.dimCol')}
                        </span>
                      )}
                      {t(`analyticsCosts.group.${dim}`)}
                    </Badge>
                  ))}
                </span>
                <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="w-48">
              <DropdownMenuLabel className="text-xs text-muted-foreground">
                {t('analyticsCosts.dimHint')}
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              {DIMENSION_OPTIONS.map((g) => (
                <DropdownMenuCheckboxItem
                  key={g}
                  checked={selectedDimensions.includes(g)}
                  onCheckedChange={() => toggleDimension(g)}
                  onSelect={(e) => e.preventDefault()}
                >
                  {t(`analyticsCosts.group.${g}`)}
                </DropdownMenuCheckboxItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
          <Button variant="outline" size="sm" disabled={exporting || rows.length === 0} onClick={handleExport}>
            <Download className="mr-1.5 h-3.5 w-3.5" />
            {t('analyticsCosts.export')}
          </Button>
          <TeamFilter teams={teams} value={selectedTeam} onChange={setSelectedTeam} />
        </div>
      </div>

      <div className={`grid gap-4 ${showPeriodCard ? 'md:grid-cols-3' : 'md:grid-cols-2'}`}>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('analyticsCosts.totalCostMtd')}</CardTitle>
            <DollarSign className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {loading ? <Skeleton className="h-8 w-24" /> : `$${new Decimal(stats.total_cost_mtd).toFixed(2)}`}
            </div>
          </CardContent>
        </Card>
        {showPeriodCard && (
          <Card>
            <CardHeader className="flex flex-row items-center justify-between pb-2">
              <CardTitle className="text-sm font-medium">
                {t('analyticsCosts.period.title', { range: t(`analyticsCosts.range_${timeRange}`) })}
              </CardTitle>
              <DollarSign className="h-4 w-4 text-muted-foreground" />
            </CardHeader>
            <CardContent>
              <div className="flex items-center gap-2">
                <div className="text-2xl font-bold">
                  {loading || periodTotal == null ? (
                    <Skeleton className="h-8 w-24" />
                  ) : (
                    `$${periodTotal.toFixed(2)}`
                  )}
                </div>
                {!loading && periodDeltaPct != null && (
                  <Badge
                    variant="secondary"
                    className={
                      periodDeltaPct.isNegative()
                        ? 'gap-1 bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300'
                        : 'gap-1 bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300'
                    }
                    aria-label={
                      periodDeltaPct.isNegative()
                        ? t('analyticsCosts.period.deltaDown', { pct: periodDeltaPct.abs().toFixed(1) })
                        : t('analyticsCosts.period.deltaUp', { pct: periodDeltaPct.toFixed(1) })
                    }
                  >
                    {periodDeltaPct.isNegative() ? (
                      <TrendingDown className="h-3 w-3" />
                    ) : (
                      <TrendingUp className="h-3 w-3" />
                    )}
                    {periodDeltaPct.isNegative()
                      ? `-${periodDeltaPct.abs().toFixed(1)}%`
                      : `+${periodDeltaPct.toFixed(1)}%`}
                  </Badge>
                )}
                {!loading && periodDeltaPct == null && periodTotal != null && (
                  <span className="text-xs text-muted-foreground">
                    {t('analyticsCosts.period.noPrior')}
                  </span>
                )}
              </div>
            </CardContent>
          </Card>
        )}
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
                  const cost = new Decimal(row.total_cost);
                  const pct = totalCost.greaterThan(0)
                    ? cost.dividedBy(totalCost).times(100).toNumber()
                    : 0;
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
                        const val = pivotData.cellMap.get(`${rk}\0${ck}`) ?? new Decimal(0);
                        return (
                          <TableCell key={ck} className="text-right tabular-nums">
                            {val.greaterThan(0) ? `$${val.toFixed(2)}` : '—'}
                          </TableCell>
                        );
                      })}
                      <TableCell className="text-right font-semibold tabular-nums">
                        ${(pivotData.rowTotals.get(rk) ?? new Decimal(0)).toFixed(2)}
                      </TableCell>
                    </TableRow>
                  ))}
                  {/* Column totals row */}
                  <TableRow className="border-t-2">
                    <TableCell className="font-semibold">{t('analyticsCosts.pivotTotal')}</TableCell>
                    {pivotData.colKeys.map((ck) => (
                      <TableCell key={ck} className="text-right font-semibold tabular-nums">
                        ${(pivotData.colTotals.get(ck) ?? new Decimal(0)).toFixed(2)}
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
