import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { BarChart3, Key, Server, Cpu, Database, MemoryStick, Search } from 'lucide-react';
import { api } from '@/lib/api';

interface HealthStatus {
  postgres: boolean;
  redis: boolean;
  clickhouse: boolean;
}

interface AuditEntry {
  id: string;
  timestamp: string;
  user_email: string;
  action: string;
  resource: string;
}

interface DashboardStats {
  total_requests_today: number;
  active_providers: number;
  active_api_keys: number;
  connected_mcp_servers: number;
}

const serviceList: { name: string; key: keyof HealthStatus; icon: typeof Database }[] = [
  { name: 'PostgreSQL', key: 'postgres', icon: Database },
  { name: 'Redis', key: 'redis', icon: MemoryStick },
  { name: 'ClickHouse', key: 'clickhouse', icon: Search },
];

export function DashboardPage() {
  const { t } = useTranslation();
  const [health, setHealth] = useState<HealthStatus | null>(null);
  const [recentActivity, setRecentActivity] = useState<AuditEntry[]>([]);
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [loadingHealth, setLoadingHealth] = useState(true);
  const [loadingActivity, setLoadingActivity] = useState(true);

  useEffect(() => {
    api<DashboardStats>('/api/dashboard/stats')
      .then(setStats)
      .catch(() => {});

    api<HealthStatus>('/api/health')
      .then(setHealth)
      .catch(() => setHealth(null))
      .finally(() => setLoadingHealth(false));

    api<{ items: AuditEntry[] }>('/api/audit/logs?limit=5')
      .then((res) => setRecentActivity(res.items ?? []))
      .catch(() => setRecentActivity([]))
      .finally(() => setLoadingActivity(false));
  }, []);

  const statCards = [
    { title: t('dashboard.totalRequests'), icon: BarChart3, value: stats?.total_requests_today, description: t('dashboard.today') },
    { title: t('dashboard.activeProviders'), icon: Cpu, value: stats?.active_providers, description: t('dashboard.configured') },
    { title: t('dashboard.apiKeysCount'), icon: Key, value: stats?.active_api_keys, description: t('dashboard.activeKeys') },
    { title: t('dashboard.mcpServersCount'), icon: Server, value: stats?.connected_mcp_servers, description: t('dashboard.connected') },
  ];

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('dashboard.title')}</h1>
        <p className="text-muted-foreground">
          {t('dashboard.subtitle')}
        </p>
      </div>

      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
        {statCards.map((stat) => (
          <Card key={stat.title}>
            <CardHeader className="flex flex-row items-center justify-between pb-2">
              <CardTitle className="text-sm font-medium">{stat.title}</CardTitle>
              <stat.icon className="h-4 w-4 text-muted-foreground" />
            </CardHeader>
            <CardContent>
              <div className="text-2xl font-bold">
                {stat.value != null ? stat.value.toLocaleString() : '...'}
              </div>
              <p className="text-xs text-muted-foreground">{stat.description}</p>
            </CardContent>
          </Card>
        ))}
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('dashboard.recentActivity')}</CardTitle>
          </CardHeader>
          <CardContent>
            {loadingActivity ? (
              <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
            ) : recentActivity.length === 0 ? (
              <p className="text-sm text-muted-foreground">{t('dashboard.noRecentActivity')}</p>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('dashboard.time')}</TableHead>
                    <TableHead>{t('dashboard.user')}</TableHead>
                    <TableHead>{t('dashboard.action')}</TableHead>
                    <TableHead>{t('dashboard.resource')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {recentActivity.map((entry) => (
                    <TableRow key={entry.id}>
                      <TableCell className="text-xs text-muted-foreground">
                        {new Date(entry.timestamp).toLocaleString()}
                      </TableCell>
                      <TableCell className="text-xs">{entry.user_email}</TableCell>
                      <TableCell className="text-xs">{entry.action}</TableCell>
                      <TableCell className="text-xs">{entry.resource}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('dashboard.systemStatus')}</CardTitle>
          </CardHeader>
          <CardContent>
            {loadingHealth ? (
              <p className="text-sm text-muted-foreground">{t('dashboard.checkingServices')}</p>
            ) : (
              <div className="space-y-3">
                {serviceList.map((svc) => {
                  const ok = health?.[svc.key] ?? false;
                  return (
                    <div key={svc.key} className="flex items-center justify-between">
                      <div className="flex items-center gap-2">
                        <svc.icon className="h-4 w-4 text-muted-foreground" />
                        <span className="text-sm font-medium">{svc.name}</span>
                      </div>
                      <Badge variant={ok ? 'default' : 'destructive'}>
                        {ok ? t('common.healthy') : t('dashboard.unreachable')}
                      </Badge>
                    </div>
                  );
                })}
              </div>
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
