import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Plus } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader } from '@/components/ui/card';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { Skeleton } from '@/components/ui/skeleton';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Textarea } from '@/components/ui/textarea';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { ApiError, api, apiDelete, apiPost } from '@/lib/api';
import type { GuardDetail, RedactException, RedactExceptionList } from '@/lib/security-types';
import { ruleName } from './names';

const PATH = '/api/admin/security/redact/exceptions';
const EXCEPTIONS_KEY = ['security', 'redact', 'exceptions'] as const;

/**
 * Redaction's exception values, under the rule table: values that are
 * never replaced or recorded, whichever rule finds them. Each row is the
 * value masked, the rule that first recognised it, when it was added, and
 * "Remove". A value is added by pasting it; the server hashes it on
 * arrival and keeps only the hash and the masked form, so the console
 * never sees it again. At the cap, adding is unavailable and says why.
 */
export function ExceptionsPanel({ detail, canWrite }: { detail: GuardDetail; canWrite: boolean }) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<RedactException | null>(null);
  const query = useQuery({
    queryKey: EXCEPTIONS_KEY,
    queryFn: ({ signal }) => api<RedactExceptionList>(PATH, { signal }),
  });
  const remove = useMutation({
    mutationFn: (e: RedactException) => apiDelete<RedactException>(`${PATH}/${e.sha256}`),
    onSuccess: (_, e) => {
      setRemoving(null);
      toast.success(t('contentSecurity.exceptions.removed', { value: e.masked }));
      void queryClient.invalidateQueries({ queryKey: EXCEPTIONS_KEY });
    },
    onError: (err) => toast.error(err instanceof Error ? err.message : t('common.error')),
  });

  const items = query.data?.items ?? [];
  const max = query.data?.max ?? 500;
  const full = items.length >= max;
  const rule = (e: RedactException) =>
    e.rule ? ruleName(i18n.language, detail, e.rule, false, e.rule) : t('contentSecurity.exceptions.noRule');

  const addButton = (
    <Button
      size="sm"
      variant="outline"
      onClick={() => setAdding(true)}
      disabled={!canWrite || full || !query.data}
      title={full ? t('contentSecurity.exceptions.full', { max }) : undefined}
    >
      <Plus />
      {t('contentSecurity.exceptions.add')}
    </Button>
  );

  return (
    <Card>
      <CardHeader>
        {items.length === 0 ? (
          // Nothing excepted: one line and the button.
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-xs text-muted-foreground">{t('contentSecurity.exceptions.empty')}</p>
            {addButton}
          </div>
        ) : (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-sm font-medium">
                {t('contentSecurity.exceptions.title', { count: items.length })}
              </h3>
              {addButton}
            </div>
            <p className="text-xs text-muted-foreground">
              {full ? t('contentSecurity.exceptions.full', { max }) : t('contentSecurity.exceptions.lead')}
            </p>
          </>
        )}
      </CardHeader>
      {(query.isPending || query.isError || items.length > 0) && (
        <CardContent>
          {query.isPending ? (
            <Skeleton className="h-8 w-full" />
          ) : query.isError ? (
            <p className="text-xs text-destructive">
              {t('contentSecurity.exceptions.loadFailed')}: {query.error.message}
            </p>
          ) : (
            <Table className="min-w-[640px] table-fixed">
              <colgroup>
                <col className="w-[30%]" />
                <col />
                <col className="w-[180px]" />
                <col className="w-[110px]" />
              </colgroup>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('contentSecurity.exceptions.col.value')}</TableHead>
                  <TableHead>{t('contentSecurity.exceptions.col.rule')}</TableHead>
                  <TableHead>{t('contentSecurity.exceptions.col.added')}</TableHead>
                  <TableHead>
                    <span className="sr-only">{t('common.actions')}</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {items.map((e) => (
                  <TableRow key={e.sha256}>
                    <TableCell className="truncate font-mono text-xs" title={e.masked}>
                      {e.masked || '—'}
                    </TableCell>
                    <TableCell className="truncate" title={rule(e)}>
                      {rule(e)}
                    </TableCell>
                    <TableCell className="text-xs tabular-nums text-muted-foreground">
                      {e.added_at ? new Date(e.added_at).toLocaleString() : '—'}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 px-2 text-xs"
                        onClick={() => setRemoving(e)}
                        disabled={!canWrite}
                      >
                        {t('contentSecurity.exceptions.remove')}
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      )}
      {adding && <AddExceptionDialog onClose={() => setAdding(false)} />}
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && !remove.isPending && setRemoving(null)}
        title={t('contentSecurity.exceptions.removeTitle', { value: removing?.masked ?? '' })}
        description={t('contentSecurity.exceptions.removeDesc')}
        confirmLabel={t('contentSecurity.exceptions.remove')}
        loading={remove.isPending}
        onConfirm={() => removing && remove.mutate(removing)}
      />
    </Card>
  );
}

/**
 * Paste a value to except it: for a test value that has not turned up in
 * the log yet. The value goes to the server once and is forgotten here as
 * soon as the dialog closes.
 */
function AddExceptionDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [value, setValue] = useState('');
  const [error, setError] = useState<string | null>(null);
  const add = useMutation({
    mutationFn: (v: string) => apiPost<RedactException>(PATH, { value: v }),
    onSuccess: (e) => {
      toast.success(t('contentSecurity.exceptions.added', { value: e.masked }));
      void queryClient.invalidateQueries({ queryKey: EXCEPTIONS_KEY });
      onClose();
    },
    onError: (err) =>
      setError(
        err instanceof ApiError && err.status === 409
          ? t('contentSecurity.exceptions.duplicate')
          : err instanceof Error
            ? err.message
            : t('common.error'),
      ),
  });
  const empty = value.trim() === '';
  return (
    <Dialog open onOpenChange={(o) => !o && !add.isPending && onClose()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('contentSecurity.exceptions.addTitle')}</DialogTitle>
          <DialogDescription>{t('contentSecurity.exceptions.addDesc')}</DialogDescription>
        </DialogHeader>
        <div className="space-y-1.5">
          <Label htmlFor="exception-value">{t('contentSecurity.exceptions.value')}</Label>
          <Textarea
            id="exception-value"
            className="min-h-20 font-mono text-xs"
            value={value}
            spellCheck={false}
            autoComplete="off"
            onChange={(e) => {
              setValue(e.target.value);
              setError(null);
            }}
          />
          <p className="text-xs text-muted-foreground">{t('contentSecurity.exceptions.valueHint')}</p>
          {error && <p className="text-xs text-destructive">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={add.isPending}>
            {t('common.cancel')}
          </Button>
          <Button onClick={() => add.mutate(value)} disabled={empty || add.isPending}>
            {t('contentSecurity.exceptions.add')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
