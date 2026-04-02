import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { FileText } from 'lucide-react';
import { api } from '@/lib/api';

interface GatewayLog {
  id: string;
  model_id: string;
  input_tokens: number;
  output_tokens: number;
  cost_usd: string;
  latency_ms: number | null;
  status_code: number | null;
  created_at: string;
}

export function GatewayLogsPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<GatewayLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState('');

  const loadLogs = useCallback(async () => {
    try {
      const params = new URLSearchParams();
      if (search) params.set('model', search);
      const data = await api<GatewayLog[]>(`/api/gateway/logs?${params}`);
      setLogs(data);
    } catch {
      // ignore
    } finally {
      setLoading(false);
    }
  }, [search]);

  useEffect(() => {
    loadLogs();
  }, [loadLogs]);

  const statusBadge = (code: number | null) => {
    if (!code) return <Badge variant="outline">—</Badge>;
    if (code >= 200 && code < 300) return <Badge variant="default">{code}</Badge>;
    if (code >= 400) return <Badge variant="destructive">{code}</Badge>;
    return <Badge variant="secondary">{code}</Badge>;
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('logs.title')}</h1>
          <p className="text-muted-foreground">{t('logs.subtitle')}</p>
        </div>
        <Input
          placeholder={t('logs.filterModel')}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="w-64"
        />
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('logs.allRequests')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('logs.noLogs')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('logs.timestamp')}</TableHead>
                  <TableHead>{t('logs.model')}</TableHead>
                  <TableHead className="text-right">{t('logs.tokensIn')}</TableHead>
                  <TableHead className="text-right">{t('logs.tokensOut')}</TableHead>
                  <TableHead className="text-right">{t('logs.cost')}</TableHead>
                  <TableHead className="text-right">{t('logs.latency')}</TableHead>
                  <TableHead>{t('logs.status')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {logs.map((log) => (
                  <TableRow key={log.id}>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(log.created_at).toLocaleString()}
                    </TableCell>
                    <TableCell className="font-mono text-sm">{log.model_id}</TableCell>
                    <TableCell className="text-right tabular-nums">{log.input_tokens.toLocaleString()}</TableCell>
                    <TableCell className="text-right tabular-nums">{log.output_tokens.toLocaleString()}</TableCell>
                    <TableCell className="text-right tabular-nums">${parseFloat(log.cost_usd).toFixed(4)}</TableCell>
                    <TableCell className="text-right tabular-nums">
                      {log.latency_ms != null ? `${log.latency_ms}ms` : '—'}
                    </TableCell>
                    <TableCell>{statusBadge(log.status_code)}</TableCell>
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
