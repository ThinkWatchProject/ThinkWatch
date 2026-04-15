import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Area, AreaChart, CartesianGrid, ReferenceLine, XAxis, YAxis } from 'recharts';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Progress } from '@/components/ui/progress';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertCircle, Coins, PhoneCall } from 'lucide-react';
import { ChartContainer, ChartTooltip, ChartTooltipContent, type ChartConfig } from '@/components/ui/chart';
import { api } from '@/lib/api';

interface LicenseTier {
  name: string;
  tokens_ceiling: number | null;
  calls_ceiling: number | null;
}

interface DailyBucket {
  date: string;
  value: number;
}

interface UsageLicenseResponse {
  month_start: string;
  billable_tokens_mtd: number;
  mcp_calls_mtd: number;
  current_tier: LicenseTier;
  next_tier: LicenseTier | null;
  tokens_daily: DailyBucket[];
  calls_daily: DailyBucket[];
}

/// Admin-only "Usage & License" dashboard. Mirrors the tier table in
/// LICENSING.md; shows month-to-date Billable Tokens and MCP Tool Calls
/// with progress bars and a per-day trend chart. Pure statistics — no
/// license activation, no phoning home.
export function UsageLicensePage() {
  const { t, i18n } = useTranslation();
  const locale = i18n.language === 'zh' ? 'zh-CN' : 'en-US';
  const [data, setData] = useState<UsageLicenseResponse | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    api<UsageLicenseResponse>('/api/admin/usage-license')
      .then((res) => {
        if (!cancelled) setData(res);
      })
      .catch((err) => {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load');
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const tokensPct = useMemo(() => {
    if (!data) return 0;
    const ceiling = data.current_tier.tokens_ceiling;
    if (ceiling == null || ceiling === 0) return 100;
    return Math.min(100, (data.billable_tokens_mtd / ceiling) * 100);
  }, [data]);

  const callsPct = useMemo(() => {
    if (!data) return 0;
    const ceiling = data.current_tier.calls_ceiling;
    if (ceiling == null || ceiling === 0) return 100;
    return Math.min(100, (data.mcp_calls_mtd / ceiling) * 100);
  }, [data]);

  const fmt = (v: number) => new Intl.NumberFormat(locale).format(v);
  const fmtShort = (v: number) =>
    new Intl.NumberFormat(locale, { notation: 'compact', maximumFractionDigits: 1 }).format(v);

  const tierBadgeVariant = (name: string): 'default' | 'secondary' | 'destructive' | 'outline' => {
    switch (name) {
      case 'Starter':
        return 'secondary';
      case 'Growth':
        return 'default';
      case 'Scale':
      case 'Enterprise':
        return 'default';
      case 'Custom':
        return 'destructive';
      default:
        return 'outline';
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('usageLicense.title')}</h1>
        <p className="text-muted-foreground">{t('usageLicense.subtitle')}</p>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle className="text-base">{t('usageLicense.currentTier')}</CardTitle>
            <CardDescription>{t('usageLicense.tierHint')}</CardDescription>
          </div>
          {loading ? (
            <Skeleton className="h-6 w-24" />
          ) : data ? (
            <Badge variant={tierBadgeVariant(data.current_tier.name)} className="text-sm">
              {t(`usageLicense.tier.${data.current_tier.name.toLowerCase()}`)}
            </Badge>
          ) : null}
        </CardHeader>
      </Card>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium flex items-center gap-2">
              <Coins className="h-4 w-4" />
              {t('usageLicense.billableTokens')}
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="text-2xl font-bold font-mono tabular-nums">
              {loading || !data ? <Skeleton className="h-8 w-32" /> : fmt(data.billable_tokens_mtd)}
            </div>
            {data && (
              <>
                <Progress value={tokensPct} />
                <p className="text-xs text-muted-foreground">
                  {data.current_tier.tokens_ceiling != null
                    ? t('usageLicense.ofCeiling', {
                        pct: tokensPct.toFixed(1),
                        ceiling: fmtShort(data.current_tier.tokens_ceiling),
                      })
                    : t('usageLicense.unboundedTier')}
                </p>
                {data.next_tier && data.next_tier.tokens_ceiling != null && (
                  <p className="text-xs text-muted-foreground">
                    {t('usageLicense.nextTierAt', {
                      tier: t(`usageLicense.tier.${data.next_tier.name.toLowerCase()}`),
                      ceiling: fmtShort(data.current_tier.tokens_ceiling ?? 0),
                    })}
                  </p>
                )}
              </>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="flex flex-row items-center justify-between pb-2">
            <CardTitle className="text-sm font-medium flex items-center gap-2">
              <PhoneCall className="h-4 w-4" />
              {t('usageLicense.mcpCalls')}
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="text-2xl font-bold font-mono tabular-nums">
              {loading || !data ? <Skeleton className="h-8 w-32" /> : fmt(data.mcp_calls_mtd)}
            </div>
            {data && (
              <>
                <Progress value={callsPct} />
                <p className="text-xs text-muted-foreground">
                  {data.current_tier.calls_ceiling != null
                    ? t('usageLicense.ofCeiling', {
                        pct: callsPct.toFixed(1),
                        ceiling: fmtShort(data.current_tier.calls_ceiling),
                      })
                    : t('usageLicense.unboundedTier')}
                </p>
              </>
            )}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('usageLicense.tokensDaily')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading || !data ? (
            <Skeleton className="h-48 w-full" />
          ) : (
            <TrendChart
              data={data.tokens_daily}
              ceiling={data.current_tier.tokens_ceiling}
              color="var(--chart-1)"
              fmt={fmtShort}
            />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('usageLicense.callsDaily')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading || !data ? (
            <Skeleton className="h-48 w-full" />
          ) : (
            <TrendChart
              data={data.calls_daily}
              ceiling={data.current_tier.calls_ceiling}
              color="var(--chart-2)"
              fmt={fmtShort}
            />
          )}
        </CardContent>
      </Card>

      <Alert>
        <AlertCircle className="h-4 w-4" />
        <AlertDescription>{t('usageLicense.localOnly')}</AlertDescription>
      </Alert>
    </div>
  );
}

function TrendChart({
  data,
  ceiling,
  color,
  fmt,
}: {
  data: DailyBucket[];
  ceiling: number | null;
  color: string;
  fmt: (v: number) => string;
}) {
  const chartData = data.map((d) => ({
    label: d.date.slice(5), // MM-DD
    value: d.value,
  }));
  const config = {
    value: { label: 'Value', color },
  } satisfies ChartConfig;

  return (
    <ChartContainer config={config} className="aspect-auto h-48 w-full">
      <AreaChart data={chartData} margin={{ top: 8, right: 16, bottom: 0, left: 0 }}>
        <defs>
          <linearGradient id="usage-fill" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--color-value)" stopOpacity={0.4} />
            <stop offset="100%" stopColor="var(--color-value)" stopOpacity={0} />
          </linearGradient>
        </defs>
        <CartesianGrid vertical={false} />
        <XAxis dataKey="label" tickLine={false} axisLine={false} fontSize={10} />
        <YAxis tickLine={false} axisLine={false} fontSize={10} tickFormatter={(v: number) => fmt(v)} />
        <ChartTooltip content={<ChartTooltipContent />} />
        {ceiling != null && (
          <ReferenceLine
            y={ceiling}
            stroke="var(--color-value)"
            strokeDasharray="4 2"
            strokeOpacity={0.5}
          />
        )}
        <Area
          dataKey="value"
          type="monotone"
          stroke="var(--color-value)"
          fill="url(#usage-fill)"
          strokeWidth={2}
        />
      </AreaChart>
    </ChartContainer>
  );
}
