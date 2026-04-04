import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Search, ChevronDown, ChevronRight, FileText, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { ScrollArea } from '@/components/ui/scroll-area';
import { api } from '@/lib/api';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';
import { Skeleton } from '@/components/ui/skeleton';

import { Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious } from '@/components/ui/pagination';

interface AuditLog {
  id: string;
  timestamp: string;
  user_id: string | null;
  user_email: string;
  action: string;
  resource: string;
  ip_address: string;
  detail: Record<string, unknown> | null;
}

interface AuditResponse {
  items: AuditLog[];
  total: number;
}

const PAGE_SIZE = 50;

export function AuditPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<AuditLog[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [page, setPage] = useState(0);
  const [expandedRow, setExpandedRow] = useState<string | null>(null);

  // Filters
  const [searchQuery, setSearchQuery] = useState('');
  const [actionFilter, setActionFilter] = useState('');
  const [resourceFilter, setResourceFilter] = useState('');
  const [userId, setUserId] = useState('');
  const [dateFrom, setDateFrom] = useState('');
  const [dateTo, setDateTo] = useState('');

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    try {
      const params = new URLSearchParams();
      if (searchQuery) params.set('q', searchQuery);
      if (actionFilter) params.set('action', actionFilter);
      if (resourceFilter) params.set('resource', resourceFilter);
      if (userId) params.set('user_id', userId);
      if (dateFrom) params.set('from', dateFrom);
      if (dateTo) params.set('to', dateTo);
      params.set('limit', String(PAGE_SIZE));
      params.set('offset', String(page * PAGE_SIZE));
      const qs = params.toString();
      const res = await api<AuditResponse>(`/api/audit/logs${qs ? `?${qs}` : ''}`);
      setLogs(res.items ?? []);
      setTotal(res.total ?? 0);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load audit logs');
    } finally {
      setLoading(false);
    }
  }, [searchQuery, actionFilter, resourceFilter, userId, dateFrom, dateTo, page]);

  useEffect(() => { fetchLogs(); }, [fetchLogs]);
  useEffect(() => { setPage(0); }, [searchQuery, actionFilter, resourceFilter, userId, dateFrom, dateTo]);

  const handleSearch = () => { setPage(0); fetchLogs(); };
  const totalPages = Math.ceil(total / PAGE_SIZE);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('audit.title')}</h1>
        <p className="text-muted-foreground">{t('audit.subtitle')}</p>
      </div>

      <Card>
        <CardContent className="pt-6">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <div>
              <Label className="text-xs">{t('common.search')}</Label>
              <Input placeholder={t('audit.searchPlaceholder')} value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && handleSearch()} />
            </div>
            <div>
              <Label className="text-xs">{t('audit.action')}</Label>
              <Input placeholder="e.g. create, delete" value={actionFilter} onChange={(e) => setActionFilter(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('audit.resource')}</Label>
              <Input placeholder="e.g. user, provider" value={resourceFilter} onChange={(e) => setResourceFilter(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('audit.user')}</Label>
              <Input placeholder="User UUID" value={userId} onChange={(e) => setUserId(e.target.value)} />
            </div>
            <div>
              <Label className="text-xs">{t('logs.dateRange', 'Date Range')}</Label>
              <DateTimeRangePicker from={dateFrom} to={dateTo} onFromChange={setDateFrom} onToChange={setDateTo} />
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

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base">{t('audit.logEntries')}</CardTitle>
          {total > 0 && (
            <span className="text-sm text-muted-foreground">{t('common.total')}: {total.toLocaleString()}</span>
          )}
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-3">
              {[...Array(5)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-8" />
                  <Skeleton className="h-4 w-28" />
                  <Skeleton className="h-4 w-36" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-20" />
                </div>
              ))}
            </div>
          ) : logs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <FileText className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('audit.noLogs')}</p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-8" />
                    <TableHead>{t('audit.timestamp')}</TableHead>
                    <TableHead>{t('audit.user')}</TableHead>
                    <TableHead>{t('audit.action')}</TableHead>
                    <TableHead>{t('audit.resource')}</TableHead>
                    <TableHead>{t('audit.ipAddress')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {logs.map((log) => (
                    <>
                      <TableRow key={log.id}>
                        <TableCell>
                          {log.detail && (
                            <Button
                              variant="ghost"
                              size="icon-xs"
                              aria-label="Toggle details"
                              onClick={() => setExpandedRow(expandedRow === log.id ? null : log.id)}
                            >
                              {expandedRow === log.id
                                ? <ChevronDown className="h-3 w-3" />
                                : <ChevronRight className="h-3 w-3" />}
                            </Button>
                          )}
                        </TableCell>
                        <TableCell className="text-xs text-muted-foreground">
                          {new Date(log.timestamp).toLocaleString()}
                        </TableCell>
                        <TableCell className="text-sm">{log.user_email}</TableCell>
                        <TableCell className="text-sm">{log.action}</TableCell>
                        <TableCell className="text-sm">{log.resource}</TableCell>
                        <TableCell className="font-mono text-xs">{log.ip_address}</TableCell>
                      </TableRow>
                      {expandedRow === log.id && log.detail && (
                        <TableRow key={`${log.id}-detail`}>
                          <TableCell colSpan={6}>
                            <div className="grid grid-cols-2 gap-2 text-xs p-2">
                              <div><span className="font-medium">User ID:</span> {log.user_id ?? '—'}</div>
                            </div>
                            <ScrollArea className="max-h-48">
                              <pre className="rounded bg-muted p-3 text-xs">
                                {JSON.stringify(log.detail, null, 2)}
                              </pre>
                            </ScrollArea>
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
