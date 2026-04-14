import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { ScrollArea } from '@/components/ui/scroll-area';
import { api } from '@/lib/api';
import type { RoleHistoryEntry } from '@/routes/admin/roles/types';

interface RoleHistoryProps {
  roleId: string;
  /** Optional: cap the visible entries (e.g. show "last 10" in the
   *  edit wizard, but full list in the dedicated detail view). */
  limit?: number;
}

/**
 * Renders the audit-log of changes to a role. Shared between the
 * read-only RoleDetail inspector and the edit wizard so an admin
 * editing a role can see who touched it last without leaving the
 * dialog. Fetches lazily from `/api/admin/roles/{id}/history`;
 * gracefully renders empty / error states.
 */
export function RoleHistory({ roleId, limit }: RoleHistoryProps) {
  const { t } = useTranslation();
  const [history, setHistory] = useState<RoleHistoryEntry[] | null>(null);
  const [error, setError] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setHistory(null);
    setError(false);
    api<{ items: RoleHistoryEntry[] }>(`/api/admin/roles/${roleId}/history`)
      .then((res) => {
        if (!cancelled) setHistory(res.items);
      })
      .catch(() => {
        if (!cancelled) setError(true);
      });
    return () => {
      cancelled = true;
    };
  }, [roleId]);

  if (history === null) {
    return (
      <p className="text-xs italic text-muted-foreground">
        {error ? t('common.error') : t('common.loading')}
      </p>
    );
  }
  if (history.length === 0) {
    return <p className="text-xs italic text-muted-foreground">{t('roles.noHistory')}</p>;
  }

  const visible = limit ? history.slice(0, limit) : history;
  const truncated = limit && history.length > limit ? history.length - limit : 0;

  return (
    <div className="space-y-2">
      <ScrollArea className="max-h-96 rounded-md border">
        <ul className="divide-y">
          {visible.map((h) => (
            <li key={h.id} className="px-3 py-2 text-xs">
              <div className="flex items-center justify-between gap-2">
                <Badge
                  variant={h.action === 'role.deleted' ? 'destructive' : 'outline'}
                  className="font-mono text-[10px]"
                >
                  {h.action}
                </Badge>
                <span className="font-mono text-[10px] text-muted-foreground">
                  {new Date(h.created_at).toLocaleString()}
                </span>
              </div>
              {(h.user_email || h.ip_address) && (
                <div className="mt-1 text-[10px] text-muted-foreground">
                  {h.user_email ?? h.user_id ?? '—'}
                  {h.ip_address && ` · ${h.ip_address}`}
                </div>
              )}
              {h.detail && (
                <pre className="mt-1 max-h-32 overflow-auto rounded bg-muted/30 p-1.5 font-mono text-[10px]">
                  {JSON.stringify(h.detail, null, 2)}
                </pre>
              )}
            </li>
          ))}
        </ul>
      </ScrollArea>
      {truncated > 0 && (
        <p className="text-[10px] italic text-muted-foreground">
          {t('roles.historyTruncated', { count: truncated })}
        </p>
      )}
    </div>
  );
}
