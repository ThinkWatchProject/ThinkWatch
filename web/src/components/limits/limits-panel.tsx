// ============================================================================
// Generic limits + budgets editor
//
// Single embeddable panel that lets an admin manage:
//
//   - Sliding-window rate-limit rules (1m / 5m / 1h / 5h / 1d / 1w)
//     filtered to the surfaces relevant to the subject
//   - Natural-period budget caps (daily / weekly / monthly) in
//     weighted tokens
//
// Both sit in a single `<details>` collapsible so the host edit
// dialog stays compact until the admin opens the section.
//
// The panel is subject-polymorphic — it takes a `(subjectKind,
// subjectId)` tuple and a list of allowed surfaces, then talks to
// the corresponding `/api/admin/limits/...` endpoints from phase D.
// Caller responsibilities:
//
//   - subjectKind: 'user' | 'api_key' | 'provider' | 'mcp_server'
//   - subjectId: the row UUID
//   - surfaces: which gateways apply to this subject. user/api_key
//     can have both, provider is ai-only, mcp_server is mcp-only.
//   - allowBudgets: whether to render the caps section. mcp_server
//     has no budget side, so the caller passes false.
//
// All four embed sites end up calling this component with one line.
// ============================================================================

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronRight, Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Progress } from '@/components/ui/progress';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { toast } from 'sonner';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { api, apiPost, apiDelete } from '@/lib/api';
// ----------------------------------------------------------------------------
// Re-export the canonical constraint types from the roles module so
// consumers can import from either location.
// ----------------------------------------------------------------------------

export type {
  ParsedConstraints as SurfaceConstraints,
  ParsedSurfaceConstraints as SurfaceBlock,
  ParsedRateLimit as SurfaceRule,
  ParsedBudget as SurfaceBudget,
} from '@/routes/admin/roles/types';

import type {
  ParsedConstraints,
  ParsedSurfaceConstraints,
  ParsedRateLimit,
  ParsedBudget,
} from '@/routes/admin/roles/types';

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

export type SubjectKind = 'role';
export type Surface = 'ai_gateway' | 'mcp_gateway';
type Metric = 'requests' | 'tokens';
type Period = 'daily' | 'weekly' | 'monthly';

interface RuleRow {
  id: string;
  subject_kind: string;
  subject_id: string;
  surface: Surface;
  metric: Metric;
  window_secs: number;
  max_count: number;
  enabled: boolean;
}

interface CapRow {
  id: string;
  subject_kind: string;
  subject_id: string;
  period: Period;
  limit_tokens: number;
  enabled: boolean;
}

interface RuleUsage {
  rule_id: string;
  current: number;
  limit: number;
}

interface CapUsage {
  cap_id: string;
  current: number;
  limit: number;
}

interface UsageResponse {
  rules: RuleUsage[];
  caps: CapUsage[];
}

interface ListResponse<T> {
  items: T[];
}

// ----------------------------------------------------------------------------
// Static option lists
//
// These mirror the closed enums on the backend (`ALLOWED_WINDOW_SECS`
// in `crates/common/src/limits/mod.rs`). Adding a new window or
// period means updating both this file and the backend constant +
// the migration CHECK constraint.
// ----------------------------------------------------------------------------

const WINDOW_OPTIONS: { secs: number; key: string; labelKey: string }[] = [
  { secs: 60, key: '1m', labelKey: 'limits.window_60' },
  { secs: 300, key: '5m', labelKey: 'limits.window_300' },
  { secs: 3600, key: '1h', labelKey: 'limits.window_3600' },
  { secs: 18000, key: '5h', labelKey: 'limits.window_18000' },
  { secs: 86400, key: '1d', labelKey: 'limits.window_86400' },
  { secs: 604800, key: '1w', labelKey: 'limits.window_604800' },
];

function windowKeyToLabel(key: string, t: TFn): string {
  const opt = WINDOW_OPTIONS.find((w) => w.key === key);
  return opt ? t(opt.labelKey) : key;
}

const PERIOD_OPTIONS: { value: Period; labelKey: string }[] = [
  { value: 'daily', labelKey: 'limits.period_daily' },
  { value: 'weekly', labelKey: 'limits.period_weekly' },
  { value: 'monthly', labelKey: 'limits.period_monthly' },
];

// ----------------------------------------------------------------------------
// Public component
// ----------------------------------------------------------------------------

interface LimitsPanelProps {
  subjectKind?: SubjectKind;
  subjectId?: string;
  /// Which gateway surfaces are valid for this subject. The rules
  /// table only shows / lets the admin add rules for these surfaces.
  surfaces: Surface[];
  /// Whether to render the budget caps section.
  allowBudgets: boolean;
  /// Controlled mode: when supplied the panel stops making API calls
  /// and edits the in-memory ParsedConstraints object instead.
  /// The parent role form collects the result and sends it as part
  /// of the role PATCH body — constraints live inline on each Statement.
  value?: ParsedConstraints;
  onChange?: (next: ParsedConstraints) => void;
  /// Strip the collapsible border/header so the panel embeds inline
  /// under a permission group without a nested-card visual.
  compact?: boolean;
}

export function LimitsPanel({
  subjectKind,
  subjectId,
  surfaces,
  allowBudgets,
  value,
  onChange,
  compact,
}: LimitsPanelProps) {
  if (value !== undefined && onChange) {
    return (
      <ControlledLimits
        surfaces={surfaces}
        allowBudgets={allowBudgets}
        value={value}
        onChange={onChange}
        compact={compact}
      />
    );
  }
  if (!subjectKind || !subjectId) {
    throw new Error('LimitsPanel requires either (subjectKind, subjectId) or (value, onChange)');
  }
  return (
    <UncontrolledLimits
      subjectKind={subjectKind}
      subjectId={subjectId}
      surfaces={surfaces}
      allowBudgets={allowBudgets}
      compact={compact}
    />
  );
}

function UncontrolledLimits({
  subjectKind,
  subjectId,
  surfaces,
  allowBudgets,
  compact,
}: {
  subjectKind: SubjectKind;
  subjectId: string;
  surfaces: Surface[];
  allowBudgets: boolean;
  compact?: boolean;
}) {
  const { t } = useTranslation();
  const [rules, setRules] = useState<RuleRow[]>([]);
  const [caps, setCaps] = useState<CapRow[]>([]);
  const [usage, setUsage] = useState<UsageResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const base = `/api/admin/limits/${subjectKind}/${subjectId}`;

  const reload = useCallback(async () => {
    setError('');
    try {
      const [r, c, u] = await Promise.all([
        api<ListResponse<RuleRow>>(`${base}/rules`),
        // Budgets endpoint 400s for mcp_server (engine doesn't accept
        // it as a budget subject). Skip the call rather than handling
        // the error.
        allowBudgets
          ? api<ListResponse<CapRow>>(`${base}/budgets`)
          : Promise.resolve({ items: [] }),
        api<UsageResponse>(`${base}/usage`),
      ]);
      setRules(r.items);
      setCaps(c.items);
      setUsage(u);
    } catch (e) {
      setError(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setLoading(false);
    }
  }, [base, allowBudgets, t]);

  useEffect(() => {
    reload();
  }, [reload]);

  return (
    <Collapsible
      defaultOpen={compact}
      className={
        compact ? 'space-y-2' : 'rounded-md border bg-muted/20 px-3 py-2'
      }
    >
      {!compact && (
        <CollapsibleTrigger asChild>
          <button
            type="button"
            className="group flex w-full cursor-pointer items-center gap-2 text-sm"
          >
            <ChevronRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-90" />
            <Label className="cursor-pointer font-medium">{t('limits.title')}</Label>
            <span className="ml-auto text-[11px] text-muted-foreground">
              {t('limits.summary', {
                rules: rules.length,
                caps: allowBudgets ? caps.length : 0,
              })}
            </span>
          </button>
        </CollapsibleTrigger>
      )}
      <CollapsibleContent className={compact ? 'space-y-3' : 'mt-3 space-y-4'}>
        {error && <p className="text-xs text-destructive">{error}</p>}
        {loading && rules.length === 0 && caps.length === 0 ? (
          <p className="text-xs italic text-muted-foreground">{t('common.loading')}</p>
        ) : (
          <>
            <RulesSection
              base={base}
              surfaces={surfaces}
              rules={rules}
              usage={usage}
              onChanged={reload}
            />
            {allowBudgets && (
              <CapsSection
                base={base}
                caps={caps}
                usage={usage}
                onChanged={reload}
              />
            )}
          </>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}

// ----------------------------------------------------------------------------
// Rules section
// ----------------------------------------------------------------------------

function RulesSection({
  base,
  surfaces,
  rules,
  usage,
  onChanged,
}: {
  base: string;
  surfaces: Surface[];
  rules: RuleRow[];
  usage: UsageResponse | null;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [deleteTarget, setDeleteTarget] = useState<RuleRow | null>(null);
  const [deleting, setDeleting] = useState(false);

  // Map rule_id → current count for the inline "X / Y" display.
  // Rules without a usage entry (newly created, never hit) show as
  // 0 / max_count.
  const usageMap = new Map<string, number>(
    (usage?.rules ?? []).map((u) => [u.rule_id, u.current]),
  );

  const confirmRemoveRule = async () => {
    if (!deleteTarget) return;
    setDeleting(true);
    try {
      await apiDelete(`${base}/rules/${deleteTarget.id}`);
      setDeleteTarget(null);
      onChanged();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setDeleting(false);
    }
  };

  const toggleEnabled = async (rule: RuleRow) => {
    try {
      await apiPost(`${base}/rules`, {
        surface: rule.surface,
        metric: rule.metric,
        window_secs: rule.window_secs,
        max_count: rule.max_count,
        enabled: !rule.enabled,
      });
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  return (
    <div className="space-y-2">
      <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
        {t('limits.rulesTitle')}
      </Label>
      <p className="text-[11px] text-muted-foreground">{t('limits.rulesHint')}</p>

      {rules.length === 0 ? (
        <p className="text-xs italic text-muted-foreground">{t('limits.noRules')}</p>
      ) : (
        <div className="rounded-md border">
          <table className="w-full text-xs">
            <thead className="border-b bg-muted/40">
              <tr className="text-left text-muted-foreground">
                <th className="px-2 py-1.5 font-medium">{t('limits.surface')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.metric')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.window')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.maxCount')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.usage')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.enabled')}</th>
                <th className="w-8" />
              </tr>
            </thead>
            <tbody className="divide-y">
              {rules.map((r) => {
                const current = usageMap.get(r.id) ?? 0;
                const pct = r.max_count > 0 ? Math.min(100, (current / r.max_count) * 100) : 0;
                return (
                  <tr key={r.id}>
                    <td className="px-2 py-1.5">
                      <Badge variant="outline" className="text-[10px]">
                        {t(`limits.surfaceShort_${r.surface}` as const)}
                      </Badge>
                    </td>
                    <td className="px-2 py-1.5 font-mono text-[10px]">{r.metric}</td>
                    <td className="px-2 py-1.5 font-mono text-[10px]">
                      {windowLabel(r.window_secs, t)}
                    </td>
                    <td className="px-2 py-1.5 font-mono tabular-nums">{r.max_count}</td>
                    <td className="px-2 py-1.5">
                      <div className="flex items-center gap-1.5">
                        <span className="font-mono tabular-nums">{current}</span>
                        <Progress
                          value={Math.min(100, pct)}
                          className={`h-1 w-12 bg-muted ${
                            pct >= 100
                              ? '[&>[data-slot=progress-indicator]]:bg-destructive'
                              : pct >= 80
                                ? '[&>[data-slot=progress-indicator]]:bg-yellow-500'
                                : ''
                          }`}
                        />
                      </div>
                    </td>
                    <td className="px-2 py-1.5">
                      <Switch
                        checked={r.enabled}
                        onCheckedChange={() => toggleEnabled(r)}
                      />
                    </td>
                    <td className="px-2 py-1.5 text-right">
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6"
                        onClick={() => setDeleteTarget(r)}
                        aria-label={t('common.delete')}
                      >
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <AddRuleRow base={base} surfaces={surfaces} onChanged={onChanged} />
      <ConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(v) => !v && setDeleteTarget(null)}
        title={t('common.delete')}
        description={
          deleteTarget
            ? t('limits.confirmDeleteRule', { label: ruleLabel(deleteTarget, t) })
            : ''
        }
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={confirmRemoveRule}
        loading={deleting}
      />
    </div>
  );
}

function AddRuleRow({
  base,
  surfaces,
  onChanged,
}: {
  base: string;
  surfaces: Surface[];
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [surface, setSurface] = useState<Surface>(surfaces[0] ?? 'ai_gateway');
  const [metric, setMetric] = useState<Metric>('requests');
  const [windowSecs, setWindowSecs] = useState<number>(3600);
  const [maxCount, setMaxCount] = useState('');
  const [busy, setBusy] = useState(false);

  // Tokens metric on the MCP gateway is meaningless (MCP calls have
  // no token cost concept). Force it to requests when the admin
  // picks the MCP surface.
  const setSurfaceWithMetric = (s: Surface) => {
    setSurface(s);
    if (s === 'mcp_gateway' && metric === 'tokens') setMetric('requests');
  };

  const submit = async () => {
    const n = parseInt(maxCount, 10);
    if (!Number.isFinite(n) || n <= 0) {
      window.alert(t('limits.maxCountInvalid'));
      return;
    }
    setBusy(true);
    try {
      await apiPost(`${base}/rules`, {
        surface,
        metric,
        window_secs: windowSecs,
        max_count: n,
        enabled: true,
      });
      setMaxCount('');
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-end gap-2 rounded-md border bg-muted/10 p-2">
      {surfaces.length > 1 && (
        <div className="space-y-1">
          <Label className="text-[10px] text-muted-foreground">{t('limits.surface')}</Label>
          <Select value={surface} onValueChange={(v) => setSurfaceWithMetric(v as Surface)}>
            <SelectTrigger className="h-7 w-32 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {surfaces.map((s) => (
                <SelectItem key={s} value={s}>
                  {t(`limits.surface_${s}` as const)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.metric')}</Label>
        <Select
          value={metric}
          onValueChange={(v) => setMetric(v as Metric)}
          disabled={surface === 'mcp_gateway'}
        >
          <SelectTrigger className="h-7 w-28 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="requests">{t('limits.metric_requests')}</SelectItem>
            <SelectItem value="tokens">{t('limits.metric_tokens')}</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.window')}</Label>
        <Select value={String(windowSecs)} onValueChange={(v) => setWindowSecs(Number(v))}>
          <SelectTrigger className="h-7 w-24 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {WINDOW_OPTIONS.map((w) => (
              <SelectItem key={w.secs} value={String(w.secs)}>
                {t(w.labelKey as 'limits.window_60')}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.maxCount')}</Label>
        <Input
          type="number"
          min={1}
          value={maxCount}
          onChange={(e) => setMaxCount(e.target.value)}
          placeholder={metric === 'tokens' ? '100000' : '60'}
          className="h-7 w-28 text-xs"
        />
      </div>
      <Button type="button" size="sm" className="h-7" onClick={submit} disabled={busy || !maxCount}>
        <Plus className="mr-1 h-3 w-3" />
        {t('limits.add')}
      </Button>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Caps section
// ----------------------------------------------------------------------------

function CapsSection({
  base,
  caps,
  usage,
  onChanged,
}: {
  base: string;
  caps: CapRow[];
  usage: UsageResponse | null;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const usageMap = new Map<string, number>(
    (usage?.caps ?? []).map((u) => [u.cap_id, u.current]),
  );

  const removeCap = async (cap: CapRow) => {
    if (
      !window.confirm(
        t('limits.confirmDeleteCap', { period: t(`limits.period_${cap.period}` as const) }),
      )
    ) {
      return;
    }
    try {
      await apiDelete(`${base}/budgets/${cap.id}`);
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  const toggleEnabled = async (cap: CapRow) => {
    try {
      await apiPost(`${base}/budgets`, {
        period: cap.period,
        limit_tokens: cap.limit_tokens,
        enabled: !cap.enabled,
      });
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
    }
  };

  return (
    <div className="space-y-2">
      <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
        {t('limits.budgetsTitle')}
      </Label>
      <p className="text-[11px] text-muted-foreground">{t('limits.budgetsHint')}</p>

      {caps.length === 0 ? (
        <p className="text-xs italic text-muted-foreground">{t('limits.noCaps')}</p>
      ) : (
        <div className="rounded-md border">
          <table className="w-full text-xs">
            <thead className="border-b bg-muted/40">
              <tr className="text-left text-muted-foreground">
                <th className="px-2 py-1.5 font-medium">{t('limits.period')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.limitTokens')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.usage')}</th>
                <th className="px-2 py-1.5 font-medium">{t('limits.enabled')}</th>
                <th className="w-8" />
              </tr>
            </thead>
            <tbody className="divide-y">
              {caps.map((c) => {
                const current = usageMap.get(c.id) ?? 0;
                const pct =
                  c.limit_tokens > 0 ? Math.min(100, (current / c.limit_tokens) * 100) : 0;
                return (
                  <tr key={c.id}>
                    <td className="px-2 py-1.5 font-mono text-[10px]">
                      {t(`limits.period_${c.period}` as const)}
                    </td>
                    <td className="px-2 py-1.5 font-mono tabular-nums">{c.limit_tokens}</td>
                    <td className="px-2 py-1.5">
                      <div className="flex items-center gap-1.5">
                        <span className="font-mono tabular-nums">{current}</span>
                        <Progress
                          value={Math.min(100, pct)}
                          className={`h-1 w-16 bg-muted ${
                            pct >= 100
                              ? '[&>[data-slot=progress-indicator]]:bg-destructive'
                              : pct >= 80
                                ? '[&>[data-slot=progress-indicator]]:bg-yellow-500'
                                : ''
                          }`}
                        />
                      </div>
                    </td>
                    <td className="px-2 py-1.5">
                      <Switch checked={c.enabled} onCheckedChange={() => toggleEnabled(c)} />
                    </td>
                    <td className="px-2 py-1.5 text-right">
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6"
                        onClick={() => removeCap(c)}
                        aria-label={t('common.delete')}
                      >
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <AddCapRow base={base} onChanged={onChanged} existingPeriods={caps.map((c) => c.period)} />
    </div>
  );
}

function AddCapRow({
  base,
  onChanged,
  existingPeriods,
}: {
  base: string;
  onChanged: () => void;
  existingPeriods: Period[];
}) {
  const { t } = useTranslation();
  // Default to the first period that doesn't already exist (the
  // backend has a UNIQUE on subject + period so re-adding the same
  // period would just overwrite).
  const defaultPeriod =
    (PERIOD_OPTIONS.map((p) => p.value).find((p) => !existingPeriods.includes(p)) ??
      'monthly') as Period;
  const [period, setPeriod] = useState<Period>(defaultPeriod);
  const [limitTokens, setLimitTokens] = useState('');
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    const n = parseInt(limitTokens, 10);
    if (!Number.isFinite(n) || n <= 0) {
      window.alert(t('limits.limitTokensInvalid'));
      return;
    }
    setBusy(true);
    try {
      await apiPost(`${base}/budgets`, {
        period,
        limit_tokens: n,
        enabled: true,
      });
      setLimitTokens('');
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-end gap-2 rounded-md border bg-muted/10 p-2">
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.period')}</Label>
        <Select value={period} onValueChange={(v) => setPeriod(v as Period)}>
          <SelectTrigger className="h-7 w-28 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {PERIOD_OPTIONS.map((p) => (
              <SelectItem key={p.value} value={p.value}>
                {t(p.labelKey as 'limits.period_daily')}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.limitTokens')}</Label>
        <Input
          type="number"
          min={1}
          value={limitTokens}
          onChange={(e) => setLimitTokens(e.target.value)}
          placeholder="1000000"
          className="h-7 w-32 text-xs"
        />
      </div>
      <Button type="button" size="sm" className="h-7" onClick={submit} disabled={busy || !limitTokens}>
        <Plus className="mr-1 h-3 w-3" />
        {t('limits.add')}
      </Button>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Controlled panel — inline editor for role surface_constraints
// ----------------------------------------------------------------------------

// Roles carry their rules / budgets inline on the row (JSONB column)
// rather than in the `rate_limit_rules` / `budget_caps` side tables,
// so the role-edit flow edits a plain in-memory object and ships it
// back as part of the role PATCH body. No `base` URL, no reload, no
// usage counters (those only exist for subjects with live traffic).
function ControlledLimits({
  surfaces,
  allowBudgets,
  value,
  onChange,
  compact,
}: {
  surfaces: Surface[];
  allowBudgets: boolean;
  value: ParsedConstraints;
  onChange: (next: ParsedConstraints) => void;
  compact?: boolean;
}) {
  const { t } = useTranslation();
  const singleSurface: Surface | null = surfaces.length === 1 ? surfaces[0] : null;

  const updateBlock = (s: Surface, next: ParsedSurfaceConstraints) => {
    const cleared = next.rateLimits.length === 0 && next.budgets.length === 0;
    const out: ParsedConstraints = { ...value };
    if (cleared) delete out[s];
    else out[s] = next;
    onChange(out);
  };

  const getBlock = (s: Surface): ParsedSurfaceConstraints =>
    value[s] ?? { rateLimits: [], budgets: [] };

  const addRule = (s: Surface, rule: ParsedRateLimit) => {
    const block = getBlock(s);
    updateBlock(s, { ...block, rateLimits: [...block.rateLimits, rule] });
  };
  const removeRule = (s: Surface, idx: number) => {
    const block = getBlock(s);
    updateBlock(s, { ...block, rateLimits: block.rateLimits.filter((_, i) => i !== idx) });
  };
  const toggleRule = (s: Surface, idx: number) => {
    const block = getBlock(s);
    updateBlock(s, {
      ...block,
      rateLimits: block.rateLimits.map((r, i) => (i === idx ? { ...r, enabled: !r.enabled } : r)),
    });
  };
  const addBudget = (s: Surface, budget: ParsedBudget) => {
    const block = getBlock(s);
    updateBlock(s, {
      ...block,
      budgets: [...block.budgets.filter((b) => b.period !== budget.period), budget],
    });
  };
  const removeBudget = (s: Surface, idx: number) => {
    const block = getBlock(s);
    updateBlock(s, { ...block, budgets: block.budgets.filter((_, i) => i !== idx) });
  };
  const toggleBudget = (s: Surface, idx: number) => {
    const block = getBlock(s);
    updateBlock(s, {
      ...block,
      budgets: block.budgets.map((b, i) => (i === idx ? { ...b, enabled: !b.enabled } : b)),
    });
  };

  const allRules: { surface: Surface; rule: ParsedRateLimit; idx: number }[] = [];
  for (const s of surfaces) {
    getBlock(s).rateLimits.forEach((rule, idx) => allRules.push({ surface: s, rule, idx }));
  }
  const allBudgets: { surface: Surface; budget: ParsedBudget; idx: number }[] = [];
  for (const s of surfaces) {
    getBlock(s).budgets.forEach((budget, idx) => allBudgets.push({ surface: s, budget, idx }));
  }

  return (
    <div className={compact ? 'space-y-3' : 'rounded-md border bg-muted/20 px-3 py-2 space-y-4'}>
      <div className="space-y-2">
        <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
          {t('limits.rulesTitle')}
        </Label>
        <p className="text-[11px] text-muted-foreground">{t('limits.rulesHint')}</p>
        {allRules.length === 0 ? (
          <p className="text-xs italic text-muted-foreground">{t('limits.noRules')}</p>
        ) : (
          <div className="rounded-md border">
            <table className="w-full text-xs">
              <thead className="border-b bg-muted/40">
                <tr className="text-left text-muted-foreground">
                  {!singleSurface && (
                    <th className="px-2 py-1.5 font-medium">{t('limits.surface')}</th>
                  )}
                  <th className="px-2 py-1.5 font-medium">{t('limits.metric')}</th>
                  <th className="px-2 py-1.5 font-medium">{t('limits.window')}</th>
                  <th className="px-2 py-1.5 font-medium">{t('limits.maxCount')}</th>
                  <th className="px-2 py-1.5 font-medium">{t('limits.enabled')}</th>
                  <th className="w-8" />
                </tr>
              </thead>
              <tbody className="divide-y">
                {allRules.map(({ surface: s, rule, idx }) => (
                  <tr key={`${s}-${idx}`}>
                    {!singleSurface && (
                      <td className="px-2 py-1.5">
                        <Badge variant="outline" className="text-[10px]">
                          {t(`limits.surfaceShort_${s}` as const)}
                        </Badge>
                      </td>
                    )}
                    <td className="px-2 py-1.5 font-mono text-[10px]">{rule.metric}</td>
                    <td className="px-2 py-1.5 font-mono text-[10px]">
                      {windowKeyToLabel(rule.window, t)}
                    </td>
                    <td className="px-2 py-1.5 font-mono tabular-nums">{rule.maxCount}</td>
                    <td className="px-2 py-1.5">
                      <Switch
                        checked={rule.enabled}
                        onCheckedChange={() => toggleRule(s, idx)}
                      />
                    </td>
                    <td className="px-2 py-1.5 text-right">
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6"
                        onClick={() => removeRule(s, idx)}
                        aria-label={t('common.delete')}
                      >
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <AddRuleInline surfaces={surfaces} onAdd={addRule} />
      </div>

      {allowBudgets && (
        <div className="space-y-2">
          <Label className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
            {t('limits.budgetsTitle')}
          </Label>
          <p className="text-[11px] text-muted-foreground">{t('limits.budgetsHint')}</p>
          {allBudgets.length === 0 ? (
            <p className="text-xs italic text-muted-foreground">{t('limits.noCaps')}</p>
          ) : (
            <div className="rounded-md border">
              <table className="w-full text-xs">
                <thead className="border-b bg-muted/40">
                  <tr className="text-left text-muted-foreground">
                    {!singleSurface && (
                      <th className="px-2 py-1.5 font-medium">{t('limits.surface')}</th>
                    )}
                    <th className="px-2 py-1.5 font-medium">{t('limits.period')}</th>
                    <th className="px-2 py-1.5 font-medium">{t('limits.limitTokens')}</th>
                    <th className="px-2 py-1.5 font-medium">{t('limits.enabled')}</th>
                    <th className="w-8" />
                  </tr>
                </thead>
                <tbody className="divide-y">
                  {allBudgets.map(({ surface: s, budget, idx }) => (
                    <tr key={`${s}-${idx}`}>
                      {!singleSurface && (
                        <td className="px-2 py-1.5">
                          <Badge variant="outline" className="text-[10px]">
                            {t(`limits.surfaceShort_${s}` as const)}
                          </Badge>
                        </td>
                      )}
                      <td className="px-2 py-1.5 font-mono text-[10px]">
                        {t(`limits.period_${budget.period}` as const)}
                      </td>
                      <td className="px-2 py-1.5 font-mono tabular-nums">{budget.maxTokens}</td>
                      <td className="px-2 py-1.5">
                        <Switch
                          checked={budget.enabled}
                          onCheckedChange={() => toggleBudget(s, idx)}
                        />
                      </td>
                      <td className="px-2 py-1.5 text-right">
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          className="h-6 w-6"
                          onClick={() => removeBudget(s, idx)}
                          aria-label={t('common.delete')}
                        >
                          <Trash2 className="h-3 w-3" />
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <AddBudgetInline surfaces={surfaces} onAdd={addBudget} />
        </div>
      )}
    </div>
  );
}

function AddRuleInline({
  surfaces,
  onAdd,
}: {
  surfaces: Surface[];
  onAdd: (s: Surface, rule: ParsedRateLimit) => void;
}) {
  const { t } = useTranslation();
  const [surface, setSurface] = useState<Surface>(surfaces[0] ?? 'ai_gateway');
  const [metric, setMetric] = useState<Metric>('requests');
  const [windowKey, setWindowKey] = useState<string>('1h');
  const [maxCount, setMaxCount] = useState('');

  const setSurfaceWithMetric = (s: Surface) => {
    setSurface(s);
    if (s === 'mcp_gateway' && metric === 'tokens') setMetric('requests');
  };

  const submit = () => {
    const n = parseInt(maxCount, 10);
    if (!Number.isFinite(n) || n <= 0) {
      window.alert(t('limits.maxCountInvalid'));
      return;
    }
    onAdd(surface, { metric, window: windowKey, maxCount: n, enabled: true });
    setMaxCount('');
  };

  return (
    <div className="flex flex-wrap items-end gap-2 rounded-md border bg-muted/10 p-2">
      {surfaces.length > 1 && (
        <div className="space-y-1">
          <Label className="text-[10px] text-muted-foreground">{t('limits.surface')}</Label>
          <Select value={surface} onValueChange={(v) => setSurfaceWithMetric(v as Surface)}>
            <SelectTrigger className="h-7 w-32 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {surfaces.map((s) => (
                <SelectItem key={s} value={s}>
                  {t(`limits.surface_${s}` as const)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.metric')}</Label>
        <Select
          value={metric}
          onValueChange={(v) => setMetric(v as Metric)}
          disabled={surface === 'mcp_gateway'}
        >
          <SelectTrigger className="h-7 w-28 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="requests">{t('limits.metric_requests')}</SelectItem>
            <SelectItem value="tokens">{t('limits.metric_tokens')}</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.window')}</Label>
        <Select value={windowKey} onValueChange={setWindowKey}>
          <SelectTrigger className="h-7 w-24 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {WINDOW_OPTIONS.map((w) => (
              <SelectItem key={w.key} value={w.key}>
                {t(w.labelKey as 'limits.window_60')}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.maxCount')}</Label>
        <Input
          type="number"
          min={1}
          value={maxCount}
          onChange={(e) => setMaxCount(e.target.value)}
          placeholder={metric === 'tokens' ? '100000' : '60'}
          className="h-7 w-28 text-xs"
        />
      </div>
      <Button type="button" size="sm" className="h-7" onClick={submit} disabled={!maxCount}>
        <Plus className="mr-1 h-3 w-3" />
        {t('limits.add')}
      </Button>
    </div>
  );
}

function AddBudgetInline({
  surfaces,
  onAdd,
}: {
  surfaces: Surface[];
  onAdd: (s: Surface, budget: ParsedBudget) => void;
}) {
  const { t } = useTranslation();
  const [surface, setSurface] = useState<Surface>(surfaces[0] ?? 'ai_gateway');
  const [period, setPeriod] = useState<Period>('monthly');
  const [limitTokens, setLimitTokens] = useState('');

  const submit = () => {
    const n = parseInt(limitTokens, 10);
    if (!Number.isFinite(n) || n <= 0) {
      window.alert(t('limits.limitTokensInvalid'));
      return;
    }
    onAdd(surface, { period, maxTokens: n, enabled: true });
    setLimitTokens('');
  };

  return (
    <div className="flex flex-wrap items-end gap-2 rounded-md border bg-muted/10 p-2">
      {surfaces.length > 1 && (
        <div className="space-y-1">
          <Label className="text-[10px] text-muted-foreground">{t('limits.surface')}</Label>
          <Select value={surface} onValueChange={(v) => setSurface(v as Surface)}>
            <SelectTrigger className="h-7 w-32 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {surfaces.map((s) => (
                <SelectItem key={s} value={s}>
                  {t(`limits.surface_${s}` as const)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.period')}</Label>
        <Select value={period} onValueChange={(v) => setPeriod(v as Period)}>
          <SelectTrigger className="h-7 w-28 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {PERIOD_OPTIONS.map((p) => (
              <SelectItem key={p.value} value={p.value}>
                {t(p.labelKey as 'limits.period_daily')}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label className="text-[10px] text-muted-foreground">{t('limits.limitTokens')}</Label>
        <Input
          type="number"
          min={1}
          value={limitTokens}
          onChange={(e) => setLimitTokens(e.target.value)}
          placeholder="1000000"
          className="h-7 w-32 text-xs"
        />
      </div>
      <Button type="button" size="sm" className="h-7" onClick={submit} disabled={!limitTokens}>
        <Plus className="mr-1 h-3 w-3" />
        {t('limits.add')}
      </Button>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

type TFn = (key: string, opts?: Record<string, unknown>) => string;

function windowLabel(secs: number, t: TFn): string {
  const opt = WINDOW_OPTIONS.find((w) => w.secs === secs);
  return opt ? t(opt.labelKey) : `${secs}s`;
}

function ruleLabel(rule: RuleRow, t: TFn): string {
  return `${t(`limits.surfaceShort_${rule.surface}` as const)} ${rule.metric} / ${windowLabel(rule.window_secs, t)}`;
}
