import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from '@tanstack/react-router';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { AlertCircle, ChevronLeft, ChevronRight, RefreshCw } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Segmented } from '@/components/segmented';
import { api } from '@/lib/api';
import { cn } from '@/lib/utils';
import { GUARDS, type Guard, type SecurityDetail } from '@/lib/security-types';
import { GUARD_ACTIONS, guardOf, outcomeOf, partOf, sourcesOf, type GuardEvent } from './log-events';
import { actionTone, guardName, ruleName } from './names';

const PAGE_SIZE = 50;

type Filter = 'all' | Guard;

/**
 * The security log: what the three guards found, newest first, read from
 * the audit log. Each row says the guard, the rule, where the hit was and
 * what became of it; monitor-mode hits say so. The request's text is never
 * in the audit log, only the trace that leads to it. An event names its
 * rule by id; the name shown is the rule view's (`detail`), in the
 * console's language.
 */
export function LogPanel({ detail }: { detail: SecurityDetail | undefined }) {
  const { t } = useTranslation();
  const [filter, setFilter] = useState<Filter>('all');
  const [page, setPage] = useState(0);

  const actions = filter === 'all' ? GUARDS.flatMap((g) => GUARD_ACTIONS[g]) : GUARD_ACTIONS[filter];
  const query = useQuery({
    queryKey: ['audit', 'logs', 'guards', { filter, page }],
    queryFn: ({ signal }) => {
      const params = new URLSearchParams({
        action: actions.join(','),
        limit: String(PAGE_SIZE),
        offset: String(page * PAGE_SIZE),
      });
      return api<{ items: GuardEvent[]; total: number }>(`/api/audit/logs?${params}`, { signal });
    },
    placeholderData: keepPreviousData,
  });
  const items = query.data?.items ?? [];
  const total = query.data?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <Segmented
            size="sm"
            label={t('contentSecurity.log.filter')}
            value={filter}
            options={[
              { value: 'all' as Filter, label: t('contentSecurity.log.all') },
              ...GUARDS.map((g) => ({ value: g as Filter, label: guardName(t, g) })),
            ]}
            onChange={(f) => {
              setFilter(f);
              setPage(0);
            }}
          />
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <span className="tabular-nums">{t('contentSecurity.log.total', { count: total })}</span>
            <Button
              variant="outline"
              size="icon-sm"
              aria-label={t('contentSecurity.log.refresh')}
              onClick={() => void query.refetch()}
              disabled={query.isFetching}
            >
              <RefreshCw className={cn(query.isFetching && 'animate-spin')} />
            </Button>
          </div>
        </div>
        <p className="text-xs text-muted-foreground">{t('contentSecurity.log.lead')}</p>
      </CardHeader>
      <CardContent className="space-y-3">
        {query.isError && !query.data ? (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>
              {t('contentSecurity.log.loadFailed')}: {query.error.message}
            </AlertDescription>
          </Alert>
        ) : query.isPending ? (
          <div className="space-y-2" role="status" aria-busy="true">
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </div>
        ) : items.length === 0 ? (
          <p className="py-8 text-center text-sm text-muted-foreground">{t('contentSecurity.log.empty')}</p>
        ) : (
          <Table className="min-w-[860px] table-fixed">
            <colgroup>
              <col className="w-[170px]" />
              <col className="w-[120px]" />
              <col />
              <col className="w-[150px]" />
              <col className="w-[150px]" />
              <col className="w-[180px]" />
              <col className="w-[90px]" />
            </colgroup>
            <TableHeader>
              <TableRow>
                <TableHead>{t('contentSecurity.log.col.time')}</TableHead>
                <TableHead>{t('contentSecurity.log.col.guard')}</TableHead>
                <TableHead>{t('contentSecurity.col.rule')}</TableHead>
                <TableHead>{t('contentSecurity.log.col.part')}</TableHead>
                <TableHead>{t('contentSecurity.log.col.outcome')}</TableHead>
                <TableHead>{t('contentSecurity.log.col.caller')}</TableHead>
                <TableHead>{t('contentSecurity.log.col.trace')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {items.map((e) => (
                <EventRow key={e.id} e={e} detail={detail} />
              ))}
            </TableBody>
          </Table>
        )}
        {pages > 1 && (
          <div className="flex items-center justify-end gap-2 text-xs text-muted-foreground">
            <span className="tabular-nums">{t('contentSecurity.log.page', { page: page + 1, pages })}</span>
            <Button
              variant="outline"
              size="icon-sm"
              aria-label={t('contentSecurity.log.previous')}
              disabled={page === 0}
              onClick={() => setPage((p) => p - 1)}
            >
              <ChevronLeft />
            </Button>
            <Button
              variant="outline"
              size="icon-sm"
              aria-label={t('contentSecurity.log.next')}
              disabled={page + 1 >= pages}
              onClick={() => setPage((p) => p + 1)}
            >
              <ChevronRight />
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function EventRow({ e, detail }: { e: GuardEvent; detail: SecurityDetail | undefined }) {
  const { t, i18n } = useTranslation();
  const guard = guardOf(e.action);
  const d = e.detail ?? {};
  if (!guard) return null;
  const outcome = outcomeOf(t, e.action, d);
  const rule = d.rule ? ruleName(i18n.language, detail?.[guard], d.rule, d.custom === true, d.rule_name) : '—';
  const sources = new Intl.ListFormat(i18n.language, { type: 'conjunction' }).format(sourcesOf(t, d));
  const when = new Date(e.created_at);
  return (
    <TableRow>
      <TableCell className="text-xs tabular-nums text-muted-foreground">
        {Number.isNaN(when.getTime()) ? e.created_at : when.toLocaleString()}
      </TableCell>
      <TableCell className="truncate">{guardName(t, guard)}</TableCell>
      <TableCell className="truncate" title={rule}>
        {rule}
        {sources && (
          <div className="truncate text-xs text-muted-foreground" title={sources}>
            {sources}
          </div>
        )}
        {d.model && <div className="truncate text-xs text-muted-foreground">{d.model}</div>}
      </TableCell>
      <TableCell className="truncate text-muted-foreground">{partOf(t, guard, d)}</TableCell>
      <TableCell className="truncate">
        <span className={actionTone(outcome.tone)}>{outcome.text}</span>
        {d.mode === 'observe' && (
          <span className="ml-1.5 text-xs text-muted-foreground">{t('contentSecurity.monitoring')}</span>
        )}
      </TableCell>
      <TableCell className="truncate text-muted-foreground" title={e.user_email ?? e.api_key_id ?? undefined}>
        {e.user_email ?? e.api_key_id ?? '—'}
      </TableCell>
      <TableCell>
        {d.trace_id ? (
          <Link
            to="/admin/trace/$traceId"
            params={{ traceId: d.trace_id }}
            className="text-xs text-primary underline-offset-2 hover:underline"
          >
            {t('contentSecurity.log.openTrace')}
          </Link>
        ) : (
          '—'
        )}
      </TableCell>
    </TableRow>
  );
}
