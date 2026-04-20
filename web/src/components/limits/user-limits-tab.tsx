// ============================================================================
// User limits tab — the "限额" page inside the user edit dialog.
//
// Single source of truth for a user's rate-limit / budget posture:
//
//   1. Effective policy table (role merge + overrides applied).
//      Each row is tagged source=role|override so operators can see
//      at a glance which slots are custom. Usage bars read live
//      from Redis counters so "is this user near their cap?" is
//      answered without a separate trip.
//
//   2. 7-day usage trend — tokens/day + requests/day from
//      gateway_logs. Keeps the "why is Alice hitting her limit
//      today?" question on the same page.
//
//   3. Audit tail — last 20 limits-related events (upsert, delete,
//      bulk apply, counter reset) where this user is either the
//      actor or the subject.
//
// Actions the component owns:
//   - Click "+ 新增覆盖" / "覆盖..." on a role row → create-override
//     drawer (prefilled from the row's slot when invoked that way).
//   - Click "编辑" on an override row → same drawer, prefilled for edit.
//   - Multi-select override rows → bulk disable / delete.
//   - Reset counter on any row → Redis DEL of the backing keys.
//
// Everything is wired to the new backend endpoints:
//   GET  /api/admin/users/{id}/limits-dashboard
//   POST /api/admin/users/{id}/limits/reset
//   POST /api/admin/limits/user/{id}/rules (+ budgets)   — single
//   POST /api/admin/limits/bulk/{rules|budgets}/{disable|delete} — bulk
// ============================================================================

import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, Plus, Trash2, PowerOff, RotateCw, Pencil } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
import { Textarea } from '@/components/ui/textarea';
import { Progress } from '@/components/ui/progress';
import { Separator } from '@/components/ui/separator';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { SimpleBarChart } from '@/components/ui/simple-chart';
import { api, apiPost } from '@/lib/api';
import { toast } from 'sonner';

// ----------------------------------------------------------------------------
// Wire types — match EffectiveRule / EffectiveCap / LimitsDashboard on
// the backend. Loose optional fields so downgraded servers still decode.
// ----------------------------------------------------------------------------

type Surface = 'ai_gateway' | 'mcp_gateway';
type Metric = 'requests' | 'tokens';
type Period = 'daily' | 'weekly' | 'monthly';
type Source = 'role' | 'override';

interface EffectiveRule {
  source: Source;
  override_id?: string | null;
  surface: Surface;
  metric: Metric;
  window_secs: number;
  max_count: number;
  role_default_max_count?: number | null;
  current: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  created_by?: string | null;
}

interface EffectiveCap {
  source: Source;
  override_id?: string | null;
  surface: Surface;
  period: Period;
  limit_tokens: number;
  role_default_limit_tokens?: number | null;
  current: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  created_by?: string | null;
}

interface UsageDay {
  day: string;
  tokens: number;
  requests: number;
}

interface AuditEvent {
  id: string;
  action: string;
  resource?: string | null;
  resource_id?: string | null;
  detail?: Record<string, unknown> | null;
  actor_user_email?: string | null;
  created_at: string;
}

interface LimitsDashboard {
  rules: EffectiveRule[];
  caps: EffectiveCap[];
  usage_7d: UsageDay[];
  recent_events: AuditEvent[];
}

// Selection key: overrides track by their row id. Rule/cap rows
// without an override (pure role) can't be selected for bulk ops —
// there's nothing to delete.
function rowKey(row: { source: Source; override_id?: string | null }): string | null {
  if (row.source !== 'override' || !row.override_id) return null;
  return row.override_id;
}

const WINDOW_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 60, labelKey: 'limits.window_60' },
  { secs: 300, labelKey: 'limits.window_300' },
  { secs: 3600, labelKey: 'limits.window_3600' },
  { secs: 18000, labelKey: 'limits.window_18000' },
  { secs: 86400, labelKey: 'limits.window_86400' },
  { secs: 604800, labelKey: 'limits.window_604800' },
];

const PERIOD_OPTIONS: Period[] = ['daily', 'weekly', 'monthly'];

const EXPIRY_PRESETS: { key: string; hours: number | null }[] = [
  { key: '4h', hours: 4 },
  { key: '24h', hours: 24 },
  { key: '7d', hours: 24 * 7 },
  { key: '30d', hours: 24 * 30 },
  { key: 'custom', hours: null },
  { key: 'permanent', hours: null },
];

// ----------------------------------------------------------------------------
// Main component
// ----------------------------------------------------------------------------

interface UserLimitsTabProps {
  userId: string;
}

interface DrawerInit {
  kind: 'rule' | 'cap';
  surface?: Surface;
  metric?: Metric;
  window_secs?: number;
  period?: Period;
  current_value?: number;
  // Non-null when editing an existing override
  editing_override_id?: string;
  // Non-null when overriding a role default (for copy in create form)
  role_default?: number;
}

export function UserLimitsTab({ userId }: UserLimitsTabProps) {
  const { t } = useTranslation();
  const [data, setData] = useState<LimitsDashboard | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [bulkAction, setBulkAction] = useState<'disable' | 'delete' | null>(null);
  const [drawer, setDrawer] = useState<DrawerInit | null>(null);
  const [resetTarget, setResetTarget] = useState<
    | { kind: 'rule'; surface: Surface; metric: Metric; window_secs: number; label: string }
    | { kind: 'cap'; period: Period; label: string }
    | null
  >(null);

  const reload = useCallback(async () => {
    setError('');
    try {
      const res = await api<LimitsDashboard>(`/api/admin/users/${userId}/limits-dashboard`);
      setData(res);
      // Drop stale selections.
      setSelected((prev) => {
        const live = new Set<string>();
        res.rules.forEach((r) => {
          const k = rowKey(r);
          if (k) live.add(k);
        });
        res.caps.forEach((c) => {
          const k = rowKey(c);
          if (k) live.add(k);
        });
        return new Set(Array.from(prev).filter((k) => live.has(k)));
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setLoading(false);
    }
  }, [userId, t]);

  useEffect(() => {
    reload();
  }, [reload]);

  const overrideKeys = useMemo(() => {
    if (!data) return { rules: new Set<string>(), caps: new Set<string>() };
    return {
      rules: new Set(
        data.rules.filter((r) => r.override_id).map((r) => r.override_id!),
      ),
      caps: new Set(data.caps.filter((c) => c.override_id).map((c) => c.override_id!)),
    };
  }, [data]);

  const someSelected = selected.size > 0;

  const toggleOne = (key: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const runBulk = async (action: 'disable' | 'delete') => {
    const ruleIds: string[] = [];
    const capIds: string[] = [];
    selected.forEach((k) => {
      if (overrideKeys.rules.has(k)) ruleIds.push(k);
      else if (overrideKeys.caps.has(k)) capIds.push(k);
    });
    const calls: Promise<unknown>[] = [];
    if (ruleIds.length > 0)
      calls.push(apiPost(`/api/admin/limits/bulk/rules/${action}`, { ids: ruleIds }));
    if (capIds.length > 0)
      calls.push(apiPost(`/api/admin/limits/bulk/budgets/${action}`, { ids: capIds }));
    try {
      await Promise.all(calls);
      toast.success(
        t('userLimitsTab.bulkSuccess', {
          action: t(`userLimitOverrides.bulk_${action}` as const),
          count: selected.size,
        }),
      );
      setSelected(new Set());
      await reload();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  const handleResetCounter = async () => {
    if (!resetTarget) return;
    try {
      const body =
        resetTarget.kind === 'rule'
          ? {
              kind: 'rule',
              surface: resetTarget.surface,
              metric: resetTarget.metric,
              window_secs: resetTarget.window_secs,
            }
          : { kind: 'cap', period: resetTarget.period };
      const res = await apiPost<{ deleted_keys: number }>(
        `/api/admin/users/${userId}/limits/reset`,
        body,
      );
      toast.success(
        t('userLimitsTab.resetSuccess', { n: res.deleted_keys }),
      );
      setResetTarget(null);
      await reload();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  return (
    <div className="space-y-5">
      {/* ------------------ Header ------------------ */}
      <div className="flex items-center justify-between gap-2">
        <div>
          <Label className="text-sm font-semibold">
            {t('userLimitsTab.effectiveTitle')}
          </Label>
          <p className="text-xs text-muted-foreground">
            {t('userLimitsTab.effectiveHint')}
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          {someSelected && (
            <>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => setBulkAction('disable')}
              >
                <PowerOff className="mr-1 h-3.5 w-3.5" />
                {t('userLimitOverrides.disableSelected', { count: selected.size })}
              </Button>
              <Button
                type="button"
                size="sm"
                variant="destructive"
                onClick={() => setBulkAction('delete')}
              >
                <Trash2 className="mr-1 h-3.5 w-3.5" />
                {t('userLimitOverrides.deleteSelected', { count: selected.size })}
              </Button>
            </>
          )}
          <Button
            type="button"
            size="sm"
            onClick={() => setDrawer({ kind: 'rule' })}
          >
            <Plus className="mr-1 h-3.5 w-3.5" />
            {t('userLimitOverrides.addOverride')}
          </Button>
        </div>
      </div>

      {error && (
        <p className="text-xs text-destructive">
          <AlertCircle className="mr-1 inline h-3 w-3" />
          {error}
        </p>
      )}

      {/* ------------------ Effective policy table ------------------ */}
      {loading ? (
        <p className="text-xs italic text-muted-foreground">
          {t('common.loading')}
        </p>
      ) : !data || (data.rules.length === 0 && data.caps.length === 0) ? (
        <p className="rounded-md border border-dashed px-3 py-6 text-center text-xs italic text-muted-foreground">
          {t('userLimitsTab.noEffectiveLimits')}
        </p>
      ) : (
        <EffectivePolicyTable
          rules={data?.rules ?? []}
          caps={data?.caps ?? []}
          selected={selected}
          onToggleSelect={toggleOne}
          onOverride={(init) => setDrawer(init)}
          onEdit={(init) => setDrawer(init)}
          onReset={(target) => setResetTarget(target)}
        />
      )}

      {/* ------------------ 7-day usage chart ------------------ */}
      {data && data.usage_7d.length > 0 && (
        <>
          <Separator />
          <section className="space-y-2">
            <Label className="text-sm font-semibold">
              {t('userLimitsTab.usageTitle')}
            </Label>
            <p className="text-xs text-muted-foreground">
              {t('userLimitsTab.usageHint')}
            </p>
            <UsageChart rows={data.usage_7d} />
          </section>
        </>
      )}

      {/* ------------------ Audit history ------------------ */}
      {data && data.recent_events.length > 0 && (
        <>
          <Separator />
          <section className="space-y-2">
            <Label className="text-sm font-semibold">
              {t('userLimitsTab.auditTitle')}
            </Label>
            <p className="text-xs text-muted-foreground">
              {t('userLimitsTab.auditHint')}
            </p>
            <AuditTable events={data.recent_events} />
          </section>
        </>
      )}

      {/* ------------------ Drawers / confirms ------------------ */}
      {drawer && (
        <OverrideDrawer
          userId={userId}
          init={drawer}
          onClose={() => setDrawer(null)}
          onApplied={() => {
            setDrawer(null);
            reload();
          }}
        />
      )}

      <ConfirmDialog
        open={bulkAction !== null}
        onOpenChange={(v) => !v && setBulkAction(null)}
        title={
          bulkAction === 'delete'
            ? t('userLimitOverrides.confirmBulkDeleteTitle')
            : t('userLimitOverrides.confirmBulkDisableTitle')
        }
        description={
          bulkAction === 'delete'
            ? t('userLimitOverrides.confirmBulkDeleteBody', { count: selected.size })
            : t('userLimitOverrides.confirmBulkDisableBody', { count: selected.size })
        }
        variant={bulkAction === 'delete' ? 'destructive' : 'default'}
        onConfirm={async () => {
          if (bulkAction) await runBulk(bulkAction);
          setBulkAction(null);
        }}
      />

      <ConfirmDialog
        open={resetTarget !== null}
        onOpenChange={(v) => !v && setResetTarget(null)}
        title={t('userLimitsTab.confirmResetTitle')}
        description={t('userLimitsTab.confirmResetBody', {
          label: resetTarget?.label ?? '',
        })}
        variant="destructive"
        confirmLabel={t('userLimitsTab.resetAction')}
        onConfirm={handleResetCounter}
      />
    </div>
  );
}

// ----------------------------------------------------------------------------
// Effective policy table
// ----------------------------------------------------------------------------

function EffectivePolicyTable({
  rules,
  caps,
  selected,
  onToggleSelect,
  onOverride,
  onEdit,
  onReset,
}: {
  rules: EffectiveRule[];
  caps: EffectiveCap[];
  selected: Set<string>;
  onToggleSelect: (key: string) => void;
  onOverride: (init: DrawerInit) => void;
  onEdit: (init: DrawerInit) => void;
  onReset: (
    target:
      | { kind: 'rule'; surface: Surface; metric: Metric; window_secs: number; label: string }
      | { kind: 'cap'; period: Period; label: string },
  ) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="rounded-md border">
      <table className="w-full text-xs">
        <thead className="border-b bg-muted/40">
          <tr className="text-left text-muted-foreground">
            <th className="w-8 px-2 py-1.5" />
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.col.source')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.col.scope')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.col.limit')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.col.usage')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.col.expires')}
            </th>
            <th className="w-28 px-2 py-1.5 text-right font-medium">
              {t('common.actions')}
            </th>
          </tr>
        </thead>
        <tbody className="divide-y">
          {rules.map((r, i) => (
            <RuleRow
              key={`rule-${r.surface}-${r.metric}-${r.window_secs}-${i}`}
              row={r}
              selected={r.override_id ? selected.has(r.override_id) : false}
              onToggleSelect={
                r.override_id ? () => onToggleSelect(r.override_id!) : undefined
              }
              onOverride={() =>
                onOverride({
                  kind: 'rule',
                  surface: r.surface,
                  metric: r.metric,
                  window_secs: r.window_secs,
                  current_value: r.max_count,
                  role_default: r.max_count,
                })
              }
              onEdit={() =>
                onEdit({
                  kind: 'rule',
                  surface: r.surface,
                  metric: r.metric,
                  window_secs: r.window_secs,
                  current_value: r.max_count,
                  editing_override_id: r.override_id ?? undefined,
                  role_default: r.role_default_max_count ?? undefined,
                })
              }
              onReset={() =>
                onReset({
                  kind: 'rule',
                  surface: r.surface,
                  metric: r.metric,
                  window_secs: r.window_secs,
                  label: `${r.surface} · ${r.metric} / ${secsLabel(r.window_secs, t)}`,
                })
              }
            />
          ))}
          {caps.map((c, i) => (
            <CapRow
              key={`cap-${c.period}-${i}`}
              row={c}
              selected={c.override_id ? selected.has(c.override_id) : false}
              onToggleSelect={
                c.override_id ? () => onToggleSelect(c.override_id!) : undefined
              }
              onOverride={() =>
                onOverride({
                  kind: 'cap',
                  period: c.period,
                  current_value: c.limit_tokens,
                  role_default: c.limit_tokens,
                })
              }
              onEdit={() =>
                onEdit({
                  kind: 'cap',
                  period: c.period,
                  current_value: c.limit_tokens,
                  editing_override_id: c.override_id ?? undefined,
                  role_default: c.role_default_limit_tokens ?? undefined,
                })
              }
              onReset={() =>
                onReset({
                  kind: 'cap',
                  period: c.period,
                  label: t(`limits.period_${c.period}` as const),
                })
              }
            />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function RuleRow({
  row,
  selected,
  onToggleSelect,
  onOverride,
  onEdit,
  onReset,
}: {
  row: EffectiveRule;
  selected: boolean;
  onToggleSelect?: () => void;
  onOverride: () => void;
  onEdit: () => void;
  onReset: () => void;
}) {
  const { t } = useTranslation();
  const pct = row.max_count > 0 ? Math.min(100, (row.current / row.max_count) * 100) : 0;
  const toneClass =
    pct >= 100
      ? '[&>[data-slot=progress-indicator]]:bg-destructive'
      : pct >= 80
        ? '[&>[data-slot=progress-indicator]]:bg-yellow-500'
        : '';
  const deltaChip = deltaLabel(row.role_default_max_count, row.max_count);
  return (
    <tr className={selected ? 'bg-muted/30' : ''}>
      <td className="px-2 py-1.5">
        {onToggleSelect ? (
          <Checkbox checked={selected} onCheckedChange={onToggleSelect} />
        ) : null}
      </td>
      <td className="px-2 py-1.5">
        <Badge variant={row.source === 'override' ? 'default' : 'secondary'}>
          {t(`userLimitsTab.source_${row.source}` as const)}
        </Badge>
      </td>
      <td className="px-2 py-1.5 font-mono text-[10px]">
        {t(`limits.surfaceShort_${row.surface}` as const)} ·{' '}
        {t(`limits.metric_${row.metric}` as const)} ·{' '}
        {secsLabel(row.window_secs, t)}
      </td>
      <td className="px-2 py-1.5 font-mono tabular-nums">
        {row.max_count.toLocaleString()}
        {deltaChip && (
          <span className="ml-1 text-[10px] text-muted-foreground">{deltaChip}</span>
        )}
      </td>
      <td className="px-2 py-1.5">
        <div className="flex items-center gap-1.5">
          <span className="font-mono tabular-nums">
            {row.current.toLocaleString()}
          </span>
          <Progress value={Math.min(100, pct)} className={`h-1 w-20 bg-muted ${toneClass}`} />
          <span className="text-[10px] text-muted-foreground">
            {pct.toFixed(0)}%
          </span>
        </div>
      </td>
      <td className="px-2 py-1.5 text-[11px]">
        <ExpiryCell at={row.expires_at} />
      </td>
      <td className="px-2 py-1.5 text-right">
        <RowActions
          source={row.source}
          onOverride={onOverride}
          onEdit={onEdit}
          onReset={onReset}
        />
      </td>
    </tr>
  );
}

function CapRow({
  row,
  selected,
  onToggleSelect,
  onOverride,
  onEdit,
  onReset,
}: {
  row: EffectiveCap;
  selected: boolean;
  onToggleSelect?: () => void;
  onOverride: () => void;
  onEdit: () => void;
  onReset: () => void;
}) {
  const { t } = useTranslation();
  const pct =
    row.limit_tokens > 0
      ? Math.min(100, (row.current / row.limit_tokens) * 100)
      : 0;
  const toneClass =
    pct >= 100
      ? '[&>[data-slot=progress-indicator]]:bg-destructive'
      : pct >= 80
        ? '[&>[data-slot=progress-indicator]]:bg-yellow-500'
        : '';
  const deltaChip = deltaLabel(row.role_default_limit_tokens, row.limit_tokens);
  return (
    <tr className={selected ? 'bg-muted/30' : ''}>
      <td className="px-2 py-1.5">
        {onToggleSelect ? (
          <Checkbox checked={selected} onCheckedChange={onToggleSelect} />
        ) : null}
      </td>
      <td className="px-2 py-1.5">
        <Badge variant={row.source === 'override' ? 'default' : 'secondary'}>
          {t(`userLimitsTab.source_${row.source}` as const)}
        </Badge>
      </td>
      <td className="px-2 py-1.5 font-mono text-[10px]">
        {t('userLimitsTab.budgetScope', {
          period: t(`limits.period_${row.period}` as const),
        })}
      </td>
      <td className="px-2 py-1.5 font-mono tabular-nums">
        {row.limit_tokens.toLocaleString()}
        {deltaChip && (
          <span className="ml-1 text-[10px] text-muted-foreground">{deltaChip}</span>
        )}
      </td>
      <td className="px-2 py-1.5">
        <div className="flex items-center gap-1.5">
          <span className="font-mono tabular-nums">
            {row.current.toLocaleString()}
          </span>
          <Progress value={Math.min(100, pct)} className={`h-1 w-20 bg-muted ${toneClass}`} />
          <span className="text-[10px] text-muted-foreground">
            {pct.toFixed(0)}%
          </span>
        </div>
      </td>
      <td className="px-2 py-1.5 text-[11px]">
        <ExpiryCell at={row.expires_at} />
      </td>
      <td className="px-2 py-1.5 text-right">
        <RowActions
          source={row.source}
          onOverride={onOverride}
          onEdit={onEdit}
          onReset={onReset}
        />
      </td>
    </tr>
  );
}

function RowActions({
  source,
  onOverride,
  onEdit,
  onReset,
}: {
  source: Source;
  onOverride: () => void;
  onEdit: () => void;
  onReset: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="inline-flex items-center gap-0.5">
      <Button
        type="button"
        size="sm"
        variant="ghost"
        className="h-6 px-1.5 text-[11px]"
        onClick={source === 'override' ? onEdit : onOverride}
        title={
          source === 'override'
            ? t('userLimitsTab.editOverride')
            : t('userLimitsTab.overrideThis')
        }
      >
        <Pencil className="mr-1 h-3 w-3" />
        {source === 'override'
          ? t('userLimitsTab.editOverride')
          : t('userLimitsTab.overrideThis')}
      </Button>
      <Button
        type="button"
        size="icon"
        variant="ghost"
        className="h-6 w-6"
        onClick={onReset}
        title={t('userLimitsTab.resetAction')}
      >
        <RotateCw className="h-3 w-3" />
      </Button>
    </div>
  );
}

function ExpiryCell({ at }: { at?: string | null }) {
  const { t } = useTranslation();
  if (!at) return <span className="text-muted-foreground">—</span>;
  const target = new Date(at);
  const ms = target.getTime() - Date.now();
  if (ms <= 0) return <Badge variant="destructive">{t('userLimitOverrides.expired')}</Badge>;
  const hours = ms / 3_600_000;
  const fmt = hours < 48 ? `${hours.toFixed(1)}h` : `${(hours / 24).toFixed(1)}d`;
  const classes = hours < 24 ? 'text-destructive' : 'text-foreground';
  return (
    <span className={classes} title={target.toLocaleString()}>
      {fmt}
    </span>
  );
}

function deltaLabel(roleDefault: number | null | undefined, effective: number): string | null {
  if (roleDefault == null || roleDefault === 0 || roleDefault === effective) return null;
  const ratio = effective / roleDefault;
  if (ratio > 1) return `↑${ratio.toFixed(1)}x`;
  if (ratio < 1) return `↓${(1 / ratio).toFixed(1)}x`;
  return null;
}

function secsLabel(secs: number, t: (k: string) => string): string {
  const opt = WINDOW_OPTIONS.find((w) => w.secs === secs);
  return opt ? t(opt.labelKey) : `${secs}s`;
}

// ----------------------------------------------------------------------------
// Usage chart (7-day token total)
// ----------------------------------------------------------------------------

function UsageChart({ rows }: { rows: UsageDay[] }) {
  const data = rows.map((r) => ({
    label: r.day.slice(5), // MM-DD
    value: r.tokens,
  }));
  return (
    <SimpleBarChart
      data={data}
      height={140}
      formatValue={(v) =>
        v >= 1_000_000
          ? `${(v / 1_000_000).toFixed(1)}M`
          : v >= 1_000
            ? `${(v / 1_000).toFixed(1)}k`
            : v.toString()
      }
    />
  );
}

// ----------------------------------------------------------------------------
// Audit tail
// ----------------------------------------------------------------------------

function AuditTable({ events }: { events: AuditEvent[] }) {
  const { t } = useTranslation();
  return (
    <div className="max-h-60 overflow-y-auto rounded-md border">
      <table className="w-full text-xs">
        <thead className="border-b bg-muted/40">
          <tr className="text-left text-muted-foreground">
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.audit.when')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.audit.action')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.audit.actor')}
            </th>
            <th className="px-2 py-1.5 font-medium">
              {t('userLimitsTab.audit.detail')}
            </th>
          </tr>
        </thead>
        <tbody className="divide-y">
          {events.map((e) => (
            <tr key={e.id}>
              <td className="px-2 py-1.5 whitespace-nowrap font-mono text-[10px]">
                {new Date(e.created_at).toLocaleString()}
              </td>
              <td className="px-2 py-1.5 font-mono text-[10px]">{e.action}</td>
              <td className="px-2 py-1.5 text-muted-foreground">
                {e.actor_user_email ?? '—'}
              </td>
              <td
                className="max-w-[18rem] truncate px-2 py-1.5 font-mono text-[10px] text-muted-foreground"
                title={e.detail ? JSON.stringify(e.detail) : ''}
              >
                {e.detail ? summarizeDetail(e.detail) : '—'}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function summarizeDetail(d: Record<string, unknown>): string {
  const parts: string[] = [];
  if (typeof d.surface === 'string') parts.push(`surface=${d.surface}`);
  if (typeof d.metric === 'string') parts.push(`metric=${d.metric}`);
  if (typeof d.period === 'string') parts.push(`period=${d.period}`);
  if (typeof d.window_secs === 'number') parts.push(`win=${d.window_secs}`);
  if (typeof d.max_count === 'number') parts.push(`max=${d.max_count}`);
  if (typeof d.limit_tokens === 'number') parts.push(`limit=${d.limit_tokens}`);
  if (typeof d.expires_at === 'string') parts.push(`exp=${d.expires_at.slice(0, 16)}`);
  if (typeof d.reason === 'string') parts.push(`reason="${d.reason}"`);
  return parts.length > 0 ? parts.join(' · ') : JSON.stringify(d);
}

// ----------------------------------------------------------------------------
// Create / edit override drawer
// ----------------------------------------------------------------------------

function OverrideDrawer({
  userId,
  init,
  onClose,
  onApplied,
}: {
  userId: string;
  init: DrawerInit;
  onClose: () => void;
  onApplied: () => void;
}) {
  const { t } = useTranslation();
  const [kind, setKind] = useState<'rule' | 'cap'>(init.kind);
  const [surface, setSurface] = useState<Surface>(init.surface ?? 'ai_gateway');
  const [metric, setMetric] = useState<Metric>(init.metric ?? 'requests');
  const [windowSecs, setWindowSecs] = useState<number>(init.window_secs ?? 3600);
  const [period, setPeriod] = useState<Period>(init.period ?? 'monthly');
  const [value, setValue] = useState<string>(
    init.current_value !== undefined ? String(init.current_value) : '',
  );
  const [expiryPreset, setExpiryPreset] = useState<string>('7d');
  const [customExpiry, setCustomExpiry] = useState('');
  const [reason, setReason] = useState('');
  const [busy, setBusy] = useState(false);

  const roleDefault = init.role_default;

  const resolveExpiry = (): string | null | 'invalid' => {
    if (expiryPreset === 'permanent') return null;
    if (expiryPreset === 'custom') {
      if (!customExpiry) return 'invalid';
      const d = new Date(customExpiry);
      if (Number.isNaN(d.getTime())) return 'invalid';
      return d.toISOString();
    }
    const preset = EXPIRY_PRESETS.find((p) => p.key === expiryPreset);
    if (!preset?.hours) return 'invalid';
    return new Date(Date.now() + preset.hours * 3_600_000).toISOString();
  };

  const submit = async () => {
    const n = parseInt(value, 10);
    if (!Number.isFinite(n) || n <= 0) {
      toast.error(t('userLimitOverrides.valueInvalid'));
      return;
    }
    const expiry = resolveExpiry();
    if (expiry === 'invalid') {
      toast.error(t('userLimitOverrides.expiryInvalid'));
      return;
    }
    setBusy(true);
    try {
      if (kind === 'rule') {
        await apiPost(`/api/admin/limits/user/${userId}/rules`, {
          surface,
          metric,
          window_secs: windowSecs,
          max_count: n,
          enabled: true,
          expires_at: expiry,
          reason: reason.trim() || null,
        });
      } else {
        await apiPost(`/api/admin/limits/user/${userId}/budgets`, {
          period,
          limit_tokens: n,
          enabled: true,
          expires_at: expiry,
          reason: reason.trim() || null,
        });
      }
      toast.success(t('userLimitOverrides.added'));
      onApplied();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  // Single-pane dialog rather than side-drawer — keeps this inside the
  // parent user-edit Dialog's existing z-index stack without layering
  // issues. When the user clicks outside we treat it as cancel.
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-[32rem] max-w-[92vw] space-y-3 rounded-md border bg-background p-4 shadow-lg"
        onClick={(e) => e.stopPropagation()}
      >
        <div>
          <h3 className="text-sm font-semibold">
            {init.editing_override_id
              ? t('userLimitsTab.drawer.editTitle')
              : t('userLimitsTab.drawer.createTitle')}
          </h3>
          {roleDefault != null && (
            <p className="text-[11px] text-muted-foreground">
              {t('userLimitsTab.drawer.roleDefaultHint', {
                value: roleDefault.toLocaleString(),
              })}
            </p>
          )}
        </div>

        <div className="flex items-center gap-3">
          <Label className="text-xs">{t('userLimitOverrides.col.type')}</Label>
          <Select value={kind} onValueChange={(v) => setKind(v as 'rule' | 'cap')}>
            <SelectTrigger className="w-32 text-xs" style={{ height: 28 }}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="rule">{t('userLimitOverrides.type_rule')}</SelectItem>
              <SelectItem value="cap">{t('userLimitOverrides.type_cap')}</SelectItem>
            </SelectContent>
          </Select>
        </div>

        {kind === 'rule' ? (
          <div className="grid gap-2 sm:grid-cols-3">
            <div className="space-y-0.5">
              <Label className="text-[10px] text-muted-foreground">
                {t('limits.surface')}
              </Label>
              <Select value={surface} onValueChange={(v) => setSurface(v as Surface)}>
                <SelectTrigger className="text-xs" style={{ height: 28 }}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="ai_gateway">
                    {t('limits.surface_ai_gateway')}
                  </SelectItem>
                  <SelectItem value="mcp_gateway">
                    {t('limits.surface_mcp_gateway')}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-0.5">
              <Label className="text-[10px] text-muted-foreground">
                {t('limits.metric')}
              </Label>
              <Select
                value={metric}
                onValueChange={(v) => setMetric(v as Metric)}
                disabled={surface === 'mcp_gateway'}
              >
                <SelectTrigger className="text-xs" style={{ height: 28 }}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="requests">
                    {t('limits.metric_requests')}
                  </SelectItem>
                  <SelectItem value="tokens">{t('limits.metric_tokens')}</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-0.5">
              <Label className="text-[10px] text-muted-foreground">
                {t('limits.window')}
              </Label>
              <Select
                value={String(windowSecs)}
                onValueChange={(v) => setWindowSecs(Number(v))}
              >
                <SelectTrigger className="text-xs" style={{ height: 28 }}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {WINDOW_OPTIONS.map((w) => (
                    <SelectItem key={w.secs} value={String(w.secs)}>
                      {t(w.labelKey)}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
        ) : (
          <div className="space-y-0.5 max-w-xs">
            <Label className="text-[10px] text-muted-foreground">
              {t('limits.period')}
            </Label>
            <Select value={period} onValueChange={(v) => setPeriod(v as Period)}>
              <SelectTrigger className="text-xs" style={{ height: 28 }}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PERIOD_OPTIONS.map((p) => (
                  <SelectItem key={p} value={p}>
                    {t(`limits.period_${p}` as const)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        )}

        <div className="grid gap-2 sm:grid-cols-2">
          <div className="space-y-0.5">
            <Label className="text-[10px] text-muted-foreground">
              {kind === 'rule' ? t('limits.maxCount') : t('limits.limitTokens')}
            </Label>
            <Input
              type="number"
              min={1}
              value={value}
              onChange={(e) => setValue(e.target.value)}
              placeholder={kind === 'rule' ? '100' : '1000000'}
              className="text-xs"
              style={{ height: 28 }}
            />
          </div>
          <div className="space-y-0.5">
            <Label className="text-[10px] text-muted-foreground">
              {t('userLimitOverrides.col.expires')}
            </Label>
            <Select value={expiryPreset} onValueChange={setExpiryPreset}>
              <SelectTrigger className="text-xs" style={{ height: 28 }}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {EXPIRY_PRESETS.map((p) => (
                  <SelectItem key={p.key} value={p.key}>
                    {t(`userLimitOverrides.expiry_${p.key}` as const)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>

        {expiryPreset === 'custom' && (
          <div className="space-y-0.5 max-w-xs">
            <Label className="text-[10px] text-muted-foreground">
              {t('userLimitOverrides.customExpiryLabel')}
            </Label>
            <Input
              type="datetime-local"
              value={customExpiry}
              onChange={(e) => setCustomExpiry(e.target.value)}
              className="text-xs"
              style={{ height: 28 }}
            />
          </div>
        )}

        <div className="space-y-0.5">
          <Label className="text-[10px] text-muted-foreground">
            {t('userLimitOverrides.col.reason')}
          </Label>
          <Textarea
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder={t('userLimitOverrides.reasonPlaceholder')}
            className="text-xs"
            rows={2}
          />
        </div>

        <div className="flex justify-end gap-2">
          <Button type="button" size="sm" variant="outline" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button type="button" size="sm" onClick={submit} disabled={busy}>
            {busy ? t('common.loading') : t('userLimitOverrides.apply')}
          </Button>
        </div>
      </div>
    </div>
  );
}
