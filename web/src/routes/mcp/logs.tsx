import { useEffect, useState, useCallback } from 'react';
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
import { FileText } from 'lucide-react';
import { api } from '@/lib/api';

interface McpLog {
  id: string;
  tool_name: string;
  server_name: string;
  user_email: string | null;
  duration_ms: number | null;
  status: string;
  error_message: string | null;
  created_at: string;
}

export function McpLogsPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<McpLog[]>([]);
  const [loading, setLoading] = useState(true);

  const loadLogs = useCallback(async () => {
    try {
      const data = await api<McpLog[]>('/api/mcp/logs');
      setLogs(data);
    } catch {
      // ignore
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadLogs();
  }, [loadLogs]);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('mcpLogs.title')}</h1>
        <p className="text-muted-foreground">{t('mcpLogs.subtitle')}</p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('mcpLogs.allCalls')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('mcpLogs.noLogs')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('mcpLogs.noLogsHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('mcpLogs.timestamp')}</TableHead>
                  <TableHead>{t('mcpLogs.tool')}</TableHead>
                  <TableHead>{t('mcpLogs.server')}</TableHead>
                  <TableHead>{t('mcpLogs.user')}</TableHead>
                  <TableHead className="text-right">{t('mcpLogs.duration')}</TableHead>
                  <TableHead>{t('mcpLogs.status')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {logs.map((log) => (
                  <TableRow key={log.id}>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(log.created_at).toLocaleString()}
                    </TableCell>
                    <TableCell className="font-mono text-sm">{log.tool_name}</TableCell>
                    <TableCell>{log.server_name}</TableCell>
                    <TableCell className="text-sm">{log.user_email ?? '—'}</TableCell>
                    <TableCell className="text-right tabular-nums">
                      {log.duration_ms != null ? `${log.duration_ms}ms` : '—'}
                    </TableCell>
                    <TableCell>
                      <Badge variant={log.status === 'success' ? 'default' : 'destructive'} title={log.error_message ?? undefined}>
                        {log.status}
                      </Badge>
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
