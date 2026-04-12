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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { BarChart3, Hash, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api } from '@/lib/api';
import { SimpleBarChart } from '@/components/ui/simple-chart';
import { Skeleton } from '@/components/ui/skeleton';

interface UsageRow {
  date: string;
  model_id: string;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  total_cost: string;
}

interface UsageStats {
  total_tokens_today: number;
  total_requests_today: number;
}

export function UsagePage() {
  const { t } = useTranslation();
  const [rows, setRows] = useState<UsageRow[]>([]);
  const [stats, setStats] = useState<UsageStats>({ total_tokens_today: 0, total_requests_today: 0 });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  // Team filter
  const [teams, setTeams] = useState<{ id: string; name: string }[]>([]);
  const [selectedTeam, setSelectedTeam] = useState<string>('');

  useEffect(() => {
    api<{ id: string; name: string }[]>('/api/admin/teams')
      .then(setTeams)
      .catch(() => []);
  }, []);

  const fetchData = useCallback((teamId: string) => {
    setLoading(true);
    const teamSuffix = teamId ? `?team_id=${teamId}` : '';
    Promise.all([
      api<UsageRow[]>(`/api/analytics/usage${teamSuffix}`),
      api<UsageStats>(`/api/analytics/usage/stats${teamSuffix}`),
    ])
      .then(([usageData, statsData]) => {
        setRows(usageData);
        setStats(statsData);
      })
      .catch((err) => setError(err instanceof Error ? err.message : 'Failed to load usage data'))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    fetchData(selectedTeam);
  }, [selectedTeam, fetchData]);

  // Aggregate tokens by date for chart
  const chartData = useMemo(() => {
    const byDate = new Map<string, number>();
    for (const row of rows) {
      byDate.set(row.date, (byDate.get(row.date) ?? 0) + row.input_tokens + row.output_tokens);
    }
    return Array.from(byDate.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([date, value]) => ({ label: date.slice(5), value })); // MM-DD
  }, [rows]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('analyticsUsage.title')}</h1>
          <p className="text-muted-foreground">{t('analyticsUsage.subtitle')}</p>
        </div>
        {teams.length > 0 && (
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">{t('analytics.teamFilter')}</span>
            <Select
              value={selectedTeam || 'all'}
              onValueChange={(v) => setSelectedTeam(v === 'all' ? '' : v)}
            >
              <SelectTrigger className="w-48">
                <SelectValue placeholder={t('analytics.allTeams')} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">{t('analytics.allTeams')}</SelectItem>
                {teams.map((team) => (
                  <SelectItem key={team.id} value={team.id}>{team.name}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        )}
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('analyticsUsage.totalTokensToday')}</CardTitle>
            <Hash className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {loading ? <Skeleton className="h-8 w-24" /> : stats.total_tokens_today.toLocaleString()}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium">{t('analyticsUsage.totalRequestsToday')}</CardTitle>
            <BarChart3 className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {loading ? <Skeleton className="h-8 w-24" /> : stats.total_requests_today.toLocaleString()}
            </div>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('analyticsUsage.tokenUsageOverTime')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <Skeleton className="h-48 w-full" />
          ) : chartData.length === 0 ? (
            <div className="flex h-48 items-center justify-center text-muted-foreground">{t('analyticsUsage.noUsage')}</div>
          ) : (
            <SimpleBarChart data={chartData} formatValue={(v) => v.toLocaleString()} />
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
          <CardTitle className="text-base">{t('analyticsUsage.usageBreakdown')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(4)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-4 w-20" />
                  <Skeleton className="h-4 w-20" />
                  <Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : rows.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <BarChart3 className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('analyticsUsage.noUsage')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('analyticsUsage.date')}</TableHead>
                  <TableHead>{t('analyticsUsage.model')}</TableHead>
                  <TableHead className="text-right">{t('analyticsUsage.requests')}</TableHead>
                  <TableHead className="text-right">{t('analyticsUsage.inputTokens')}</TableHead>
                  <TableHead className="text-right">{t('analyticsUsage.outputTokens')}</TableHead>
                  <TableHead className="text-right">{t('analyticsUsage.totalCost')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((row, i) => (
                  <TableRow key={`${row.date}-${row.model_id}-${i}`}>
                    <TableCell className="text-xs">{row.date}</TableCell>
                    <TableCell className="font-mono text-xs">{row.model_id}</TableCell>
                    <TableCell className="text-right">{row.request_count.toLocaleString()}</TableCell>
                    <TableCell className="text-right">{row.input_tokens.toLocaleString()}</TableCell>
                    <TableCell className="text-right">{row.output_tokens.toLocaleString()}</TableCell>
                    <TableCell className="text-right">${parseFloat(row.total_cost).toFixed(4)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
