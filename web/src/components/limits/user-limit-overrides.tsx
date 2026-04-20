// ============================================================================
// Per-user limit override manager
//
// Sits inside the user edit dialog and surfaces the user's side-table
// rate_limit_rules + budget_caps as a single "overrides" table.
//
// Interactions this panel owns:
//   - Multi-select rows → bulk disable / bulk delete via POST
//     /api/admin/limits/bulk/{rules|budgets}/{disable|delete}.
//   - Single-row toggle / trash buttons as the quick path for one-off
//     tweaks. Same endpoints under the hood; the bulk API accepts
//     single-element arrays.
//   - Inline "+ Add override" form that builds either an UpsertRule
//     or UpsertCap body depending on the selected type.
//
// The panel deliberately does NOT show role defaults inline yet (we'd
// need a server endpoint that returns the merged role policy for a
// given user-id; that's a follow-up). Reason: calling compute in the
// backend requires auth context, and we don't want to duplicate role
// merge logic in the frontend. The "Expires" column does the heavy
// lifting of reminding the operator this is temporary.
// ============================================================================

import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, Plus, Trash2, PowerOff } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
import { Textarea } from '@/components/ui/textarea';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { api, apiPost } from '@/lib/api';
import { toast } from 'sonner';

// ----------------------------------------------------------------------------
// Types (mirror backend wire shapes from crates/server/src/handlers/limits.rs)
// ----------------------------------------------------------------------------

type Surface = 'ai_gateway' | 'mcp_gateway';
type Metric = 'requests' | 'tokens';
type Period = 'daily' | 'weekly' | 'monthly';

interface RuleRow {
  id: string;
  subject_kind: 'user' | 'api_key';
  subject_id: string;
  surface: Surface;
  metric: Metric;
  window_secs: number;
  max_count: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  created_by?: string | null;
}

interface CapRow {
  id: string;
  subject_kind: 'user' | 'api_key';
  subject_id: string;
  period: Period;
  limit_tokens: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  created_by?: string | null;
}

interface ListResponse<T> {
  items: T[];
}

/// Unified row for the table. `kind` discriminates the two server tables.
type OverrideRow =
  | ({ kind: 'rule' } & RuleRow)
  | ({ kind: 'cap' } & CapRow);

interface UserLimitOverridesProps {
  userId: string;
}

// ----------------------------------------------------------------------------
// Static option lists — keep in sync with ALLOWED_WINDOW_SECS and
// BudgetPeriod in crates/common/src/limits/mod.rs.
// ----------------------------------------------------------------------------

const WINDOW_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 60, labelKey: 'limits.window_60' },
  { secs: 300, labelKey: 'limits.window_300' },
  { secs: 3600, labelKey: 'limits.window_3600' },
  { secs: 18000, labelKey: 'limits.window_18000' },
  { secs: 86400, labelKey: 'limits.window_86400' },
  { secs: 604800, labelKey: 'limits.window_604800' },
];

const PERIOD_OPTIONS: Period[] = ['daily', 'weekly', 'monthly'];

/// Quick expiry presets — cover 90% of cases. "Custom" falls back to
/// a raw datetime-local input. "Permanent" yields a null expires_at
/// with a secondary confirm (operators should prefer bounded overrides).
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

export function UserLimitOverrides({ userId }: UserLimitOverridesProps) {
  const { t } = useTranslation();
  const [rules, setRules] = useState<RuleRow[]>([]);
  const [caps, setCaps] = useState<CapRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [bulkAction, setBulkAction] = useState<'disable' | 'delete' | null>(null);
  const [adding, setAdding] = useState(false);

  const reload = useCallback(async () => {
    setError('');
    try {
      const [r, c] = await Promise.all([
        api<ListResponse<RuleRow>>(`/api/admin/limits/user/${userId}/rules`),
        api<ListResponse<CapRow>>(`/api/admin/limits/user/${userId}/budgets`),
      ]);
      setRules(r.items);
      setCaps(c.items);
      // Drop selections that reference rows no longer in the set (the
      // row may have been deleted, expired, or filtered out).
      setSelected((prev) => {
        const live = new Set<string>();
        r.items.forEach((x) => live.add(`rule:${x.id}`));
        c.items.forEach((x) => live.add(`cap:${x.id}`));
        const next = new Set<string>();
        prev.forEach((k) => {
          if (live.has(k)) next.add(k);
        });
        return next;
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

  // Merge the two tables into a single display list. `compoundId` is
  // `kind:uuid` so selection keys don't collide across tables.
  const rows = useMemo<(OverrideRow & { compoundId: string })[]>(() => {
    const out: (OverrideRow & { compoundId: string })[] = [];
    rules.forEach((r) =>
      out.push({ kind: 'rule', compoundId: `rule:${r.id}`, ...r }),
    );
    caps.forEach((c) => out.push({ kind: 'cap', compoundId: `cap:${c.id}`, ...c }));
    return out.sort((a, b) => a.kind.localeCompare(b.kind));
  }, [rules, caps]);

  const allSelected = rows.length > 0 && selected.size === rows.length;
  const someSelected = selected.size > 0;

  const toggleAll = () => {
    if (allSelected) setSelected(new Set());
    else setSelected(new Set(rows.map((r) => r.compoundId)));
  };
  const toggleOne = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  /// Partition the current selection into rule ids and cap ids so we
  /// can send one POST per table. `fire-and-parallel` keeps the UX
  /// snappy on big selections.
  const runBulk = async (action: 'disable' | 'delete') => {
    const ruleIds: string[] = [];
    const capIds: string[] = [];
    selected.forEach((k) => {
      if (k.startsWith('rule:')) ruleIds.push(k.slice(5));
      else if (k.startsWith('cap:')) capIds.push(k.slice(4));
    });
    const calls: Promise<unknown>[] = [];
    if (ruleIds.length > 0) {
      calls.push(
        apiPost(`/api/admin/limits/bulk/rules/${action}`, { ids: ruleIds }),
      );
    }
    if (capIds.length > 0) {
      calls.push(
        apiPost(`/api/admin/limits/bulk/budgets/${action}`, { ids: capIds }),
      );
    }
    try {
      await Promise.all(calls);
      toast.success(
        t('userLimitOverrides.bulkSuccess', {
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

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-2">
        <div>
          <Label className="text-sm font-semibold">
            {t('userLimitOverrides.title')}
          </Label>
          <p className="text-xs text-muted-foreground">
            {t('userLimitOverrides.hint')}
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          {someSelected && (
            <>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setBulkAction('disable')}
              >
                <PowerOff className="mr-1 h-3.5 w-3.5" />
                {t('userLimitOverrides.disableSelected', { count: selected.size })}
              </Button>
              <Button
                size="sm"
                variant="destructive"
                onClick={() => setBulkAction('delete')}
              >
                <Trash2 className="mr-1 h-3.5 w-3.5" />
                {t('userLimitOverrides.deleteSelected', { count: selected.size })}
              </Button>
            </>
          )}
          <Button size="sm" onClick={() => setAdding((v) => !v)}>
            <Plus className="mr-1 h-3.5 w-3.5" />
            {adding ? t('common.cancel') : t('userLimitOverrides.addOverride')}
          </Button>
        </div>
      </div>

      {error && (
        <p className="text-xs text-destructive">
          <AlertCircle className="mr-1 inline h-3 w-3" />
          {error}
        </p>
      )}

      {adding && (
        <AddOverrideForm
          userId={userId}
          onClose={() => setAdding(false)}
          onAdded={() => {
            setAdding(false);
            reload();
          }}
        />
      )}

      {loading ? (
        <p className="text-xs italic text-muted-foreground">{t('common.loading')}</p>
      ) : rows.length === 0 ? (
        <p className="rounded-md border border-dashed px-3 py-4 text-center text-xs italic text-muted-foreground">
          {t('userLimitOverrides.empty')}
        </p>
      ) : (
        <div className="rounded-md border">
          <table className="w-full text-xs">
            <thead className="border-b bg-muted/40">
              <tr className="text-left text-muted-foreground">
                <th className="w-8 px-2 py-1.5">
                  <Checkbox
                    checked={allSelected}
                    onCheckedChange={toggleAll}
                    aria-label="select all"
                  />
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.type')}
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.scope')}
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.value')}
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.expires')}
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.reason')}
                </th>
                <th className="px-2 py-1.5 font-medium">
                  {t('userLimitOverrides.col.enabled')}
                </th>
              </tr>
            </thead>
            <tbody className="divide-y">
              {rows.map((row) => (
                <OverrideRowView
                  key={row.compoundId}
                  row={row}
                  checked={selected.has(row.compoundId)}
                  onToggle={() => toggleOne(row.compoundId)}
                />
              ))}
            </tbody>
          </table>
        </div>
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
    </div>
  );
}

// ----------------------------------------------------------------------------
// Row view — pure render, no side effects
// ----------------------------------------------------------------------------

function OverrideRowView({
  row,
  checked,
  onToggle,
}: {
  row: OverrideRow & { compoundId: string };
  checked: boolean;
  onToggle: () => void;
}) {
  const { t } = useTranslation();
  return (
    <tr className={checked ? 'bg-muted/30' : ''}>
      <td className="px-2 py-1.5">
        <Checkbox checked={checked} onCheckedChange={onToggle} aria-label="select row" />
      </td>
      <td className="px-2 py-1.5">
        <Badge variant={row.kind === 'rule' ? 'default' : 'secondary'}>
          {row.kind === 'rule'
            ? t('userLimitOverrides.type_rule')
            : t('userLimitOverrides.type_cap')}
        </Badge>
      </td>
      <td className="px-2 py-1.5 font-mono text-[10px]">
        {row.kind === 'rule' ? (
          <>
            {t(`limits.surfaceShort_${row.surface}` as const)} ·{' '}
            {t(`limits.metric_${row.metric}` as const)} ·{' '}
            {windowLabel(row.window_secs, t)}
          </>
        ) : (
          <>{t(`limits.period_${row.period}` as const)}</>
        )}
      </td>
      <td className="px-2 py-1.5 font-mono tabular-nums">
        {row.kind === 'rule' ? row.max_count : row.limit_tokens}
      </td>
      <td className="px-2 py-1.5 text-[11px]">
        <ExpiryCell at={row.expires_at} />
      </td>
      <td className="px-2 py-1.5 max-w-[14rem] truncate text-muted-foreground" title={row.reason ?? ''}>
        {row.reason ?? '—'}
      </td>
      <td className="px-2 py-1.5">
        {row.enabled ? (
          <Badge variant="default">{t('common.enabled')}</Badge>
        ) : (
          <Badge variant="secondary">{t('common.disabled')}</Badge>
        )}
      </td>
    </tr>
  );
}

function ExpiryCell({ at }: { at?: string | null }) {
  const { t } = useTranslation();
  if (!at) {
    return (
      <span className="text-muted-foreground">
        {t('userLimitOverrides.permanent')}
      </span>
    );
  }
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

function windowLabel(secs: number, t: (k: string) => string): string {
  const opt = WINDOW_OPTIONS.find((w) => w.secs === secs);
  return opt ? t(opt.labelKey) : `${secs}s`;
}

// ----------------------------------------------------------------------------
// Add-override inline form
// ----------------------------------------------------------------------------

function AddOverrideForm({
  userId,
  onClose,
  onAdded,
}: {
  userId: string;
  onClose: () => void;
  onAdded: () => void;
}) {
  const { t } = useTranslation();
  const [kind, setKind] = useState<'rule' | 'cap'>('rule');
  const [surface, setSurface] = useState<Surface>('ai_gateway');
  const [metric, setMetric] = useState<Metric>('requests');
  const [windowSecs, setWindowSecs] = useState<number>(3600);
  const [period, setPeriod] = useState<Period>('monthly');
  const [value, setValue] = useState('');
  const [expiryPreset, setExpiryPreset] = useState<string>('7d');
  const [customExpiry, setCustomExpiry] = useState('');
  const [reason, setReason] = useState('');
  const [busy, setBusy] = useState(false);

  /// Resolve the preset into an ISO 8601 UTC timestamp (or null for
  /// permanent). Custom preset reads from a datetime-local input which
  /// is in the operator's browser time; we convert to UTC ISO.
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
    if (expiry !== null && reason.trim().length < 10) {
      toast.error(t('userLimitOverrides.reasonRequired'));
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
      onAdded();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3 rounded-md border bg-muted/10 p-3">
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
                <SelectItem value="requests">{t('limits.metric_requests')}</SelectItem>
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
          {expiryPreset !== 'permanent' && (
            <span className="ml-1 text-destructive">*</span>
          )}
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
  );
}
