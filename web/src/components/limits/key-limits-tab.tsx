// ============================================================================
// API key limits tab — the "Limits" tab inside the API key edit dialog.
//
// The key's own rate-limit rules and budgets, each with what it has used,
// read live from the counters enforcement uses. Requests made with the key
// are held to these on top of its owner's limits (the user page's limits
// tab). The rules and budgets are stored against the key's lineage, so
// they follow the key across rotations.
//
// Reading needs `rate_limits:read`, changing `rate_limits:write`, each in a
// scope that covers the key; without write the tab is read-only. Adding
// and editing reuse the user tab's drawer with `kind = api_key`:
//
//   POST   /api/admin/limits/api_key/{id}/rules     (+ budgets)
//   DELETE /api/admin/limits/api_key/{id}/rules/{rule_id}  (+ budgets/{cap_id})
// ============================================================================

import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { AlertCircle, Pencil, Plus, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Label } from '@/components/ui/label';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { apiDelete } from '@/lib/api';
import {
  ExpiryCell,
  LimitDrawer,
  RuleScopeLabel,
  UsageMeter,
  type DrawerInit,
} from './user-limits-tab';
import {
  keyLimitsQuery,
  keyLimitsQueryKey,
  type KeyCap,
  type KeyRule,
} from './key-limits-query';

interface KeyLimitsTabProps {
  keyId: string;
  /** The caller may add, edit and delete (`rate_limits:write`). */
  canEdit: boolean;
}

export function KeyLimitsTab({ keyId, canEdit }: KeyLimitsTabProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const query = useQuery(keyLimitsQuery(keyId));
  const [drawer, setDrawer] = useState<DrawerInit | null>(null);
  const [deleting, setDeleting] = useState<
    { kind: 'rule'; id: string } | { kind: 'cap'; id: string } | null
  >(null);

  const reload = () => queryClient.invalidateQueries({ queryKey: keyLimitsQueryKey(keyId) });

  const handleDelete = async () => {
    if (!deleting) return;
    const path = deleting.kind === 'rule' ? 'rules' : 'budgets';
    try {
      await apiDelete(`/api/admin/limits/api_key/${keyId}/${path}/${deleting.id}`);
      toast.success(t('keyLimits.deleted'));
      setDeleting(null);
      await reload();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  const data = query.data;
  const empty = !!data && data.rules.length === 0 && data.caps.length === 0;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <div>
          <Label className="text-sm font-semibold">{t('keyLimits.title')}</Label>
          <p className="text-xs text-muted-foreground">{t('keyLimits.hint')}</p>
        </div>
        {canEdit && (
          <Button type="button" size="sm" onClick={() => setDrawer({ kind: 'rule' })}>
            <Plus className="mr-1 h-3.5 w-3.5" />
            {t('keyLimits.add')}
          </Button>
        )}
      </div>

      {query.error && (
        <p className="text-xs text-destructive">
          <AlertCircle className="mr-1 inline h-3 w-3" />
          {query.error.message}
        </p>
      )}

      {query.isPending ? (
        <p className="text-xs italic text-muted-foreground">{t('common.loading')}</p>
      ) : empty ? (
        <p className="rounded-md border border-dashed px-3 py-6 text-center text-xs italic text-muted-foreground">
          {t('keyLimits.none')}
        </p>
      ) : data ? (
        <div className="rounded-md border">
          <table className="w-full text-xs">
            <thead className="border-b bg-muted/40">
              <tr className="text-left text-muted-foreground">
                <th className="px-2 py-1.5 font-medium">{t('userLimitsTab.col.scope')}</th>
                <th className="px-2 py-1.5 font-medium">{t('userLimitsTab.col.limit')}</th>
                <th className="px-2 py-1.5 font-medium">{t('userLimitsTab.col.usage')}</th>
                <th className="px-2 py-1.5 font-medium">{t('userLimitsTab.col.expires')}</th>
                {canEdit && (
                  <th className="w-24 px-2 py-1.5 text-right font-medium">
                    {t('common.actions')}
                  </th>
                )}
              </tr>
            </thead>
            <tbody className="divide-y">
              {data.rules.map((r) => (
                <LimitRow
                  key={`rule-${r.id}`}
                  scope={
                    <RuleScopeLabel
                      surface={r.surface}
                      metric={r.metric}
                      windowSecs={r.window_secs}
                    />
                  }
                  limit={r.max_count}
                  current={r.current}
                  enabled={r.enabled}
                  expiresAt={r.expires_at}
                  canEdit={canEdit}
                  onEdit={() => setDrawer(ruleInit(r))}
                  onDelete={() => setDeleting({ kind: 'rule', id: r.id })}
                />
              ))}
              {data.caps.map((c) => (
                <LimitRow
                  key={`cap-${c.id}`}
                  scope={t('userLimitsTab.budgetScope', {
                    period: t(`limits.period_${c.period}` as const),
                  })}
                  limit={c.limit_tokens}
                  current={c.current}
                  enabled={c.enabled}
                  expiresAt={c.expires_at}
                  canEdit={canEdit}
                  onEdit={() => setDrawer(capInit(c))}
                  onDelete={() => setDeleting({ kind: 'cap', id: c.id })}
                />
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      {drawer && (
        <LimitDrawer
          subject={{ kind: 'api_key', id: keyId }}
          init={drawer}
          onClose={() => setDrawer(null)}
          onApplied={() => {
            setDrawer(null);
            reload();
          }}
        />
      )}

      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(v) => !v && setDeleting(null)}
        title={t('keyLimits.confirmDeleteTitle')}
        description={t('keyLimits.confirmDeleteBody')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={handleDelete}
      />
    </div>
  );
}

function ruleInit(r: KeyRule): DrawerInit {
  return {
    kind: 'rule',
    surface: r.surface,
    metric: r.metric,
    window_secs: r.window_secs,
    current_value: r.max_count,
    editing_override_id: r.id,
    expires_at: r.expires_at ?? null,
    reason: r.reason ?? null,
  };
}

function capInit(c: KeyCap): DrawerInit {
  return {
    kind: 'cap',
    period: c.period,
    current_value: c.limit_tokens,
    editing_override_id: c.id,
    expires_at: c.expires_at ?? null,
    reason: c.reason ?? null,
  };
}

function LimitRow({
  scope,
  limit,
  current,
  enabled,
  expiresAt,
  canEdit,
  onEdit,
  onDelete,
}: {
  scope: ReactNode;
  limit: number;
  current: number;
  enabled: boolean;
  expiresAt?: string | null;
  canEdit: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  return (
    <tr className={enabled ? '' : 'text-muted-foreground'}>
      <td className="px-2 py-1.5 font-mono text-[10px]">
        {scope}
        {!enabled && (
          <Badge variant="secondary" className="ml-1.5 text-[10px]">
            {t('keyLimits.disabled')}
          </Badge>
        )}
      </td>
      <td className="px-2 py-1.5 font-mono tabular-nums">{limit.toLocaleString()}</td>
      <td className="px-2 py-1.5">
        <UsageMeter current={current} limit={limit} />
      </td>
      <td className="px-2 py-1.5 text-[11px]">
        <ExpiryCell at={expiresAt} />
      </td>
      {canEdit && (
        <td className="px-2 py-1.5 text-right">
          <div className="inline-flex items-center gap-0.5">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-6 px-1.5 text-[11px]"
              onClick={onEdit}
            >
              <Pencil className="mr-1 h-3 w-3" />
              {t('common.edit')}
            </Button>
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="h-6 w-6"
              onClick={onDelete}
              title={t('common.delete')}
              aria-label={t('common.delete')}
            >
              <Trash2 className="h-3 w-3" />
            </Button>
          </div>
        </td>
      )}
    </tr>
  );
}
