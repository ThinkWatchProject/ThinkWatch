import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Search, FileText, ChevronDown, ChevronRight as ChevronRightIcon } from 'lucide-react';
import { ScrollArea } from '@/components/ui/scroll-area';
import { api } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious } from '@/components/ui/pagination';

interface McpLog {
  id: string;
  user_id: string | null;
  tool_name: string;
  server_id: string | null;
  server_name: string;
  user_email: string | null;
  duration_ms: number | null;
  status: string;
  error_message: string | null;
  detail: Record<string, unknown> | null;
  ip_address: string | null;
  created_at: string;
}

interface McpLogsResponse {
  items: McpLog[];
  total: number;
}

const PAGE_SIZE = 50;

export function McpLogsPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<McpLog[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [page, setPage] = useState(0);
  const [expandedRow, setExpandedRow] = useState<string | null>(null);

  // Filters
  const [query, setQuery] = useState('');
  const [toolName, setToolName] = useState('');
  const [serverId, setServerId] = useState('');
  const [userId, setUserId] = useState('');
  const [status, setStatus] = useState('');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [sortBy, setSortBy] = useState('created_at');

  const loadLogs = useCallback(async () => {
    setLoading(true);
    try {
      const params = new URLSearchParams();
      if (query) params.set('q', query);
      if (toolName) params.set('tool_name', toolName);
      if (serverId) params.set('server_id', serverId);
      if (userId) params.set('user_id', userId);
      if (status) params.set('status', status);
      if (from) params.set('from', from);
      if (to) params.set('to', to);
      if (sortBy !== 'created_at') params.set('sort', sortBy);
      params.set('limit', String(PAGE_SIZE));
      params.set('offset', String(page * PAGE_SIZE));
      const data = await api<McpLogsResponse>(`/api/mcp/logs?${params}`);
      setLogs(data.items);
      setTotal(data.total);
    } catch {
      setLogs([]);
      setTotal(0);
    } finally {
      setLoading(false);
    }
  }, [query, toolName, serverId, userId, status, from, to, sortBy, page]);

  useEffect(() => { loadLogs(); }, [loadLogs]);
  useEffect(() => { setPage(0); }, [query, toolName, serverId, userId, status, from, to, sortBy]);

  const totalPages = Math.ceil(total / PAGE_SIZE);

  const handleSearch = () => { setPage(0); loadLogs(); };

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('mcpLogs.title')}</h1>
        <p className="text-muted-foreground">{t('mcpLogs.subtitle')}</p>
      </div>

      {/* Search filters */}
      <Card>
        <CardContent className="pt-6">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <div>
              <Label className="text-xs">{t('common.search')}</Label>
              <Input placeholder={t('mcpLogs.searchPlaceholder')} value={query} onChange={(e) => setQuery(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && handleSearch()} />
            </div>
            <div>
              <Label className="text-xs">{t('mcpLogs.tool')}</Label>
              <Input placeholder="tool_name" value={toolName} onChange={(e) => setToolName(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('mcpLogs.server')}</Label>
              <Input placeholder="server UUID" value={serverId} onChange={(e) => setServerId(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('mcpLogs.status')}</Label>
              <Select value={status || '__all__'} onValueChange={(v) => setStatus(v === '__all__' ? '' : v)}>
                <SelectTrigger className="h-8"><SelectValue placeholder={t('common.all')} /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="__all__">{t('common.all')}</SelectItem>
                  <SelectItem value="success">Success</SelectItem>
                  <SelectItem value="error">Error</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div>
              <Label className="text-xs">{t('mcpLogs.user')}</Label>
              <Input placeholder="UUID" value={userId} onChange={(e) => setUserId(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('logs.dateRange', 'Date Range')}</Label>
              <DateTimeRangePicker from={from} to={to} onFromChange={setFrom} onToChange={setTo} />
            </div>
            <div>
              <Label className="text-xs">{t('logs.sortBy')}</Label>
              <Select value={sortBy} onValueChange={(v) => setSortBy(v ?? 'created_at')}>
                <SelectTrigger className="h-8"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="created_at">{t('logs.timestamp')}</SelectItem>
                  <SelectItem value="duration_ms">{t('mcpLogs.duration')}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
          <div className="mt-3 flex justify-end">
            <Button variant="outline" onClick={handleSearch}>
              <Search className="mr-1.5 h-4 w-4" />
              {t('common.search')}
            </Button>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base">{t('mcpLogs.allCalls')}</CardTitle>
          {total > 0 && (
            <span className="text-sm text-muted-foreground">
              {t('common.total')}: {total.toLocaleString()}
            </span>
          )}
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(5)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-8" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-5 w-14 rounded-full" />
                  <Skeleton className="h-4 w-20" />
                  <Skeleton className="h-4 w-16" />
                </div>
              ))}
            </div>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('mcpLogs.noLogs')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('mcpLogs.noLogsHint')}</p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-8" />
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
                    <>
                      <TableRow key={log.id}>
                        <TableCell>
                          <Button variant="ghost" size="icon-xs" aria-label="Toggle details"
                            onClick={() => setExpandedRow(expandedRow === log.id ? null : log.id)}>
                            {expandedRow === log.id
                              ? <ChevronDown className="h-3 w-3" />
                              : <ChevronRightIcon className="h-3 w-3" />}
                          </Button>
                        </TableCell>
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
                      {expandedRow === log.id && (
                        <TableRow key={`${log.id}-detail`}>
                          <TableCell colSpan={7}>
                            <div className="grid grid-cols-2 gap-2 text-xs p-2">
                              <div><span className="font-medium">User ID:</span> {log.user_id ?? '—'}</div>
                              <div><span className="font-medium">Server ID:</span> {log.server_id ?? '—'}</div>
                              <div><span className="font-medium">IP:</span> {log.ip_address ?? '—'}</div>
                              {log.error_message && <div className="col-span-2 text-destructive"><span className="font-medium">Error:</span> {log.error_message}</div>}
                            </div>
                            {log.detail && (
                              <ScrollArea className="max-h-48">
                                <pre className="rounded bg-muted p-3 text-xs">
                                  {JSON.stringify(log.detail, null, 2)}
                                </pre>
                              </ScrollArea>
                            )}
                          </TableCell>
                        </TableRow>
                      )}
                    </>
                  ))}
                </TableBody>
              </Table>
              {totalPages > 1 && (
                <div className="flex items-center justify-between pt-4">
                  <span className="text-sm text-muted-foreground">
                    {page * PAGE_SIZE + 1}–{Math.min((page + 1) * PAGE_SIZE, total)} / {total}
                  </span>
                  <Pagination className="mx-0 w-auto">
                    <PaginationContent>
                      <PaginationItem>
                        <PaginationPrevious
                          text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page - 1); }}
                          aria-disabled={page === 0}
                          className={page === 0 ? 'pointer-events-none opacity-50' : ''}
                        />
                      </PaginationItem>
                      <PaginationItem>
                        <span className="text-sm px-2">{page + 1} / {totalPages}</span>
                      </PaginationItem>
                      <PaginationItem>
                        <PaginationNext
                          text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page + 1); }}
                          aria-disabled={page >= totalPages - 1}
                          className={page >= totalPages - 1 ? 'pointer-events-none opacity-50' : ''}
                        />
                      </PaginationItem>
                    </PaginationContent>
                  </Pagination>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
