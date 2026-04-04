import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Search } from 'lucide-react';
import { api } from '@/lib/api';

interface PlatformLogEntry {
  id: string;
  user_id: string | null;
  action: string;
  resource: string | null;
  resource_id: string | null;
  detail: Record<string, unknown> | null;
  ip_address: string | null;
  user_agent: string | null;
  created_at: string;
}

interface PlatformLogsResponse {
  items: PlatformLogEntry[];
  total: number;
}

export function PlatformLogsPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<PlatformLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);

  const [query, setQuery] = useState('');
  const [userId, setUserId] = useState('');
  const [action, setAction] = useState('');
  const [resource, setResource] = useState('');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    try {
      const params = new URLSearchParams();
      if (query) params.set('q', query);
      if (userId) params.set('user_id', userId);
      if (action) params.set('action', action);
      if (resource) params.set('resource', resource);
      if (from) params.set('from', from);
      if (to) params.set('to', to);
      params.set('limit', '50');
      const data = await api<PlatformLogsResponse>(`/api/admin/platform-logs?${params}`);
      setLogs(data.items);
      setTotal(data.total);
    } catch {
      // silently fail
    } finally {
      setLoading(false);
    }
  }, [query, userId, action, resource, from, to]);

  useEffect(() => {
    fetchLogs();
  }, [fetchLogs]);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('platformLogs.title')}</h1>
        <p className="text-muted-foreground">{t('platformLogs.subtitle')}</p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('platformLogs.filters')}</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <div>
              <Label>{t('common.search')}</Label>
              <Input
                placeholder={t('platformLogs.searchPlaceholder')}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
            <div>
              <Label>{t('platformLogs.userId')}</Label>
              <Input value={userId} onChange={(e) => setUserId(e.target.value)} placeholder="UUID" />
            </div>
            <div>
              <Label>{t('audit.action')}</Label>
              <Input value={action} onChange={(e) => setAction(e.target.value)} placeholder="admin.create_user" />
            </div>
            <div>
              <Label>{t('audit.resource')}</Label>
              <Input value={resource} onChange={(e) => setResource(e.target.value)} placeholder="user" />
            </div>
            <div>
              <Label>{t('audit.from')}</Label>
              <Input type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
            </div>
            <div>
              <Label>{t('audit.to')}</Label>
              <Input type="date" value={to} onChange={(e) => setTo(e.target.value)} />
            </div>
          </div>
          <Button className="mt-3" onClick={fetchLogs}>
            <Search className="mr-2 h-4 w-4" />
            {t('common.search')}
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('platformLogs.logEntries')} ({total})
          </CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
          ) : logs.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">{t('platformLogs.noLogs')}</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('audit.timestamp')}</TableHead>
                  <TableHead>{t('audit.user')}</TableHead>
                  <TableHead>{t('audit.action')}</TableHead>
                  <TableHead>{t('audit.resource')}</TableHead>
                  <TableHead>{t('audit.ipAddress')}</TableHead>
                  <TableHead>{t('audit.detail')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {logs.map((log) => (
                  <TableRow key={log.id}>
                    <TableCell className="text-xs whitespace-nowrap">
                      {new Date(log.created_at).toLocaleString()}
                    </TableCell>
                    <TableCell className="font-mono text-xs">{log.user_id?.slice(0, 8) ?? '—'}</TableCell>
                    <TableCell className="text-xs">{log.action}</TableCell>
                    <TableCell className="text-xs">{log.resource ?? '—'}{log.resource_id ? `:${log.resource_id.slice(0, 8)}` : ''}</TableCell>
                    <TableCell className="text-xs text-muted-foreground">{log.ip_address ?? '—'}</TableCell>
                    <TableCell className="text-xs max-w-48 truncate text-muted-foreground">
                      {log.detail ? JSON.stringify(log.detail) : '—'}
                    </TableCell>
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
