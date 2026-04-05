import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { subHours, format } from 'date-fns';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Search, FileText, ChevronDown, ChevronRight } from 'lucide-react';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { api } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';
import { Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious } from '@/components/ui/pagination';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type LogCategory = 'gateway' | 'mcp' | 'audit' | 'platform' | 'access' | 'app';

interface LogEntry {
  id: string;
  created_at: string;
  [key: string]: unknown;
}

interface LogsResponse {
  items: LogEntry[];
  total: number;
}

const CATEGORY_API: Record<LogCategory, string> = {
  gateway: '/api/gateway/logs',
  mcp: '/api/mcp/logs',
  audit: '/api/audit/logs',
  platform: '/api/admin/platform-logs',
  access: '/api/admin/access-logs',
  app: '/api/admin/app-logs',
};

const PAGE_SIZE = 50;

// ---------------------------------------------------------------------------
// Query syntax parser: "level:error target:auth some text" →
//   { level: "error", target: "auth", q: "some text" }
// ---------------------------------------------------------------------------

function parseQuery(input: string): Record<string, string> {
  const params: Record<string, string> = {};
  const freeText: string[] = [];
  // Match key:value (value can be quoted)
  const regex = /(\w+):(?:"([^"]*)"|(\S+))/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = regex.exec(input)) !== null) {
    // Collect text before this match as free text
    const before = input.slice(lastIndex, match.index).trim();
    if (before) freeText.push(before);
    lastIndex = regex.lastIndex;
    const key = match[1];
    const value = match[2] ?? match[3];
    params[key] = value;
  }
  const after = input.slice(lastIndex).trim();
  if (after) freeText.push(after);
  if (freeText.length > 0) params.q = freeText.join(' ');
  return params;
}

// ---------------------------------------------------------------------------
// Column definitions per category
// ---------------------------------------------------------------------------

interface ColDef {
  key: string;
  label: string;
  align?: 'right';
  mono?: boolean;
  render?: (v: unknown, row: LogEntry) => React.ReactNode;
}

function statusBadge(code: unknown) {
  const c = Number(code);
  if (!c) return <Badge variant="outline">—</Badge>;
  if (c >= 200 && c < 300) return <Badge variant="default">{c}</Badge>;
  if (c >= 400) return <Badge variant="destructive">{c}</Badge>;
  return <Badge variant="secondary">{c}</Badge>;
}

function levelBadge(level: unknown) {
  const l = String(level).toUpperCase();
  if (l === 'ERROR') return <Badge variant="destructive">{l}</Badge>;
  if (l === 'WARN') return <Badge className="bg-yellow-600 text-white">{l}</Badge>;
  if (l === 'DEBUG' || l === 'TRACE') return <Badge variant="secondary">{l}</Badge>;
  return <Badge variant="outline">{l}</Badge>;
}

function getColumns(cat: LogCategory): ColDef[] {
  switch (cat) {
    case 'gateway':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'model_id', label: 'Model', mono: true },
        { key: 'provider', label: 'Provider' },
        { key: 'input_tokens', label: 'In', align: 'right' },
        { key: 'output_tokens', label: 'Out', align: 'right' },
        { key: 'cost_usd', label: 'Cost', align: 'right', render: (v) => `$${parseFloat(String(v || 0)).toFixed(4)}` },
        { key: 'latency_ms', label: 'Latency', align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status_code', label: 'Status', render: (v) => statusBadge(v) },
      ];
    case 'mcp':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'tool_name', label: 'Tool', mono: true },
        { key: 'server_name', label: 'Server' },
        { key: 'duration_ms', label: 'Duration', align: 'right', render: (v) => v != null ? `${v}ms` : '—' },
        { key: 'status', label: 'Status', render: (v) => <Badge variant={v === 'success' ? 'default' : 'destructive'}>{String(v)}</Badge> },
        { key: 'user_email', label: 'User' },
      ];
    case 'audit':
      return [
        { key: 'timestamp', label: 'Time' },
        { key: 'user_email', label: 'User' },
        { key: 'action', label: 'Action' },
        { key: 'resource', label: 'Resource' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'platform':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'user_email', label: 'User' },
        { key: 'action', label: 'Action' },
        { key: 'resource', label: 'Resource' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'access':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'method', label: 'Method' },
        { key: 'path', label: 'Path', mono: true },
        { key: 'status_code', label: 'Status', render: (v) => statusBadge(v) },
        { key: 'latency_ms', label: 'Latency', align: 'right', render: (v) => `${v}ms` },
        { key: 'port', label: 'Port' },
        { key: 'ip_address', label: 'IP', mono: true },
      ];
    case 'app':
      return [
        { key: 'created_at', label: 'Time' },
        { key: 'level', label: 'Level', render: (v) => levelBadge(v) },
        { key: 'target', label: 'Target', mono: true },
        { key: 'message', label: 'Message' },
        { key: 'span', label: 'Span' },
      ];
  }
}

function getTimeKey(cat: LogCategory): string {
  return cat === 'audit' ? 'timestamp' : 'created_at';
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function UnifiedLogsPage() {
  const { t } = useTranslation();
  const [category, setCategory] = useState<LogCategory>('platform');
  const [searchInput, setSearchInput] = useState('');
  const [activeQuery, setActiveQuery] = useState('');
  const [from, setFrom] = useState(() => format(subHours(new Date(), 1), "yyyy-MM-dd'T'HH:mm"));
  const [to, setTo] = useState(() => format(new Date(), "yyyy-MM-dd'T'HH:mm"));
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [page, setPage] = useState(0);
  const [expandedRow, setExpandedRow] = useState<string | null>(null);

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const parsed = parseQuery(activeQuery);
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(parsed)) {
        if (v) params.set(k, v);
      }
      if (from) params.set('from', from.replace('T', ' ') + ':00');
      if (to) params.set('to', to.replace('T', ' ') + ':00');
      params.set('limit', String(PAGE_SIZE));
      params.set('offset', String(page * PAGE_SIZE));
      const qs = params.toString();
      const url = `${CATEGORY_API[category]}${qs ? `?${qs}` : ''}`;
      const res = await api<LogsResponse>(url);
      setLogs(res.items ?? []);
      setTotal(res.total ?? 0);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load logs');
      setLogs([]);
      setTotal(0);
    } finally {
      setLoading(false);
    }
  }, [category, activeQuery, from, to, page]);

  useEffect(() => { fetchLogs(); }, [fetchLogs]);

  const handleSearch = () => {
    setPage(0);
    setActiveQuery(searchInput);
  };

  const handleCategoryChange = (v: string) => {
    setCategory(v as LogCategory);
    setPage(0);
    setExpandedRow(null);
  };

  const totalPages = Math.ceil(total / PAGE_SIZE);
  const columns = getColumns(category);
  const timeKey = getTimeKey(category);

  const placeholders: Record<LogCategory, string> = {
    gateway: 'model:gpt-4o status_code:200 provider:openai',
    mcp: 'tool_name:search status:error server_id:xxx',
    audit: 'action:create resource:provider user_id:xxx',
    platform: 'action:role.deleted resource:role user_id:xxx',
    access: 'method:POST path:/api/admin status_code:500 port:3001',
    app: 'level:error target:audit message text',
  };

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('unifiedLogs.title', 'Logs')}</h1>
        <p className="text-muted-foreground">{t('unifiedLogs.subtitle', 'Unified log explorer')}</p>
      </div>

      <div className="flex gap-2 items-center">
        <Select value={category} onValueChange={handleCategoryChange}>
          <SelectTrigger className="w-36 shrink-0">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="platform">{t('unifiedLogs.platform', 'Platform')}</SelectItem>
            <SelectItem value="audit">{t('unifiedLogs.audit', 'Audit')}</SelectItem>
            <SelectItem value="gateway">{t('unifiedLogs.gateway', 'Gateway')}</SelectItem>
            <SelectItem value="mcp">{t('unifiedLogs.mcp', 'MCP')}</SelectItem>
            <SelectItem value="access">{t('unifiedLogs.access', 'Access')}</SelectItem>
            <SelectItem value="app">{t('unifiedLogs.app', 'App')}</SelectItem>
          </SelectContent>
        </Select>
        <Input
          placeholder={placeholders[category]}
          value={searchInput}
          onChange={(e) => setSearchInput(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && handleSearch()}
          className="flex-1 font-mono text-sm"
        />
        <div className="shrink-0 w-64">
          <DateTimeRangePicker from={from} to={to} onFromChange={setFrom} onToChange={setTo} />
        </div>
        <Button onClick={handleSearch} className="shrink-0">
          <Search className="h-4 w-4 mr-1" />
          {t('common.search')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between py-3">
          <CardTitle className="text-base">{t('unifiedLogs.results', 'Results')}</CardTitle>
          {total > 0 && (
            <span className="text-sm text-muted-foreground">{t('common.total')}: {total.toLocaleString()}</span>
          )}
        </CardHeader>
        <CardContent className="p-0">
          {loading ? (
            <div className="space-y-3 p-6">
              {[...Array(5)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-8" />
                  <Skeleton className="h-4 w-28" />
                  <Skeleton className="h-4 w-36" />
                  <Skeleton className="h-4 w-24" />
                </div>
              ))}
            </div>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-16 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('unifiedLogs.noLogs', 'No logs found.')}</p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-8" />
                    {columns.map((col) => (
                      <TableHead key={col.key} className={col.align === 'right' ? 'text-right' : ''}>
                        {col.label}
                      </TableHead>
                    ))}
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {logs.map((log) => {
                    const rowTime = String(log[timeKey] ?? log.created_at ?? '');
                    return (
                      <>
                        <TableRow key={log.id}>
                          <TableCell>
                            <Button variant="ghost" size="icon-xs" aria-label="Expand"
                              onClick={() => setExpandedRow(expandedRow === log.id ? null : log.id)}>
                              {expandedRow === log.id
                                ? <ChevronDown className="h-3 w-3" />
                                : <ChevronRight className="h-3 w-3" />}
                            </Button>
                          </TableCell>
                          {columns.map((col) => {
                            const val = log[col.key];
                            let display: React.ReactNode;
                            if (col.render) {
                              display = col.render(val, log);
                            } else if (col.key === timeKey || col.key === 'created_at' || col.key === 'timestamp') {
                              display = rowTime ? new Date(rowTime).toLocaleString() : '—';
                            } else {
                              display = val != null ? String(val) : '—';
                            }
                            return (
                              <TableCell key={col.key}
                                className={`text-sm ${col.align === 'right' ? 'text-right tabular-nums' : ''} ${col.mono ? 'font-mono' : ''}`}>
                                {display}
                              </TableCell>
                            );
                          })}
                        </TableRow>
                        {expandedRow === log.id && (
                          <TableRow key={`${log.id}-detail`}>
                            <TableCell colSpan={columns.length + 1}>
                              <ScrollArea className="max-h-64">
                                <pre className="rounded bg-muted p-3 text-xs whitespace-pre-wrap">
                                  {JSON.stringify(log, null, 2)}
                                </pre>
                              </ScrollArea>
                            </TableCell>
                          </TableRow>
                        )}
                      </>
                    );
                  })}
                </TableBody>
              </Table>
              {totalPages > 1 && (
                <div className="flex items-center justify-between p-4 border-t">
                  <span className="text-sm text-muted-foreground">
                    {page * PAGE_SIZE + 1}–{Math.min((page + 1) * PAGE_SIZE, total)} / {total}
                  </span>
                  <Pagination className="mx-0 w-auto">
                    <PaginationContent>
                      <PaginationItem>
                        <PaginationPrevious text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page - 1); }}
                          aria-disabled={page === 0}
                          className={page === 0 ? 'pointer-events-none opacity-50' : ''} />
                      </PaginationItem>
                      <PaginationItem>
                        <span className="text-sm px-2">{page + 1} / {totalPages}</span>
                      </PaginationItem>
                      <PaginationItem>
                        <PaginationNext text=""
                          onClick={(e: React.MouseEvent) => { e.preventDefault(); setPage(page + 1); }}
                          aria-disabled={page >= totalPages - 1}
                          className={page >= totalPages - 1 ? 'pointer-events-none opacity-50' : ''} />
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
