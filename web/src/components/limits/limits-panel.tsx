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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { api, apiPost, apiDelete } from '@/lib/api';

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

export type SubjectKind = 'user' | 'api_key' | 'provider' | 'mcp_server' | 'team';
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

const WINDOW_OPTIONS: { secs: number; labelKey: string }[] = [
  { secs: 60, labelKey: 'limits.window_60' },
  { secs: 300, labelKey: 'limits.window_300' },
  { secs: 3600, labelKey: 'limits.window_3600' },
  { secs: 18000, labelKey: 'limits.window_18000' },
  { secs: 86400, labelKey: 'limits.window_86400' },
  { secs: 604800, labelKey: 'limits.window_604800' },
];

const PERIOD_OPTIONS: { value: Period; labelKey: string }[] = [
  { value: 'daily', labelKey: 'limits.period_daily' },
  { value: 'weekly', labelKey: 'limits.period_weekly' },
  { value: 'monthly', labelKey: 'limits.period_monthly' },
];

// ----------------------------------------------------------------------------
// Public component
// ----------------------------------------------------------------------------

interface LimitsPanelProps {
  subjectKind: SubjectKind;
  subjectId: string;
  /// Which gateway surfaces are valid for this subject. The rules
  /// table only shows / lets the admin add rules for these surfaces.
  ///
  ///   - user, api_key with both surfaces → ['ai_gateway', 'mcp_gateway']
  ///   - api_key restricted to AI       → ['ai_gateway']
  ///   - provider                       → ['ai_gateway']
  ///   - mcp_server                     → ['mcp_gateway']
  surfaces: Surface[];
  /// Whether to render the budget caps section. mcp_server has no
  /// budget side (the backend `BudgetSubject` enum doesn't include
  /// it) so the caller passes false there.
  allowBudgets: boolean;
}

export function LimitsPanel({
  subjectKind,
  subjectId,
  surfaces,
  allowBudgets,
}: LimitsPanelProps) {
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
    <details className="rounded-md border bg-muted/20 px-3 py-2 [&[open]>summary>svg]:rotate-90">
      <summary className="flex cursor-pointer items-center gap-2 text-sm">
        <ChevronRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform" />
        <Label className="cursor-pointer font-medium">{t('limits.title')}</Label>
        <span className="ml-auto text-[11px] text-muted-foreground">
          {t('limits.summary', {
            rules: rules.length,
            caps: allowBudgets ? caps.length : 0,
          })}
        </span>
      </summary>
      <div className="mt-3 space-y-4">
        {error && (
          <p className="text-xs text-destructive">{error}</p>
        )}
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
      </div>
    </details>
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

  // Map rule_id → current count for the inline "X / Y" display.
  // Rules without a usage entry (newly created, never hit) show as
  // 0 / max_count.
  const usageMap = new Map<string, number>(
    (usage?.rules ?? []).map((u) => [u.rule_id, u.current]),
  );

  const removeRule = async (rule: RuleRow) => {
    if (
      !window.confirm(
        t('limits.confirmDeleteRule', { label: ruleLabel(rule, t) }),
      )
    ) {
      return;
    }
    try {
      await apiDelete(`${base}/rules/${rule.id}`);
      onChanged();
    } catch (e) {
      window.alert(e instanceof Error ? e.message : t('common.operationFailed'));
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
                        <div className="h-1 w-12 overflow-hidden rounded bg-muted">
                          <div
                            className={`h-full ${pct >= 100 ? 'bg-destructive' : pct >= 80 ? 'bg-yellow-500' : 'bg-primary'}`}
                            style={{ width: `${pct}%` }}
                          />
                        </div>
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
                        onClick={() => removeRule(r)}
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
                        <div className="h-1 w-16 overflow-hidden rounded bg-muted">
                          <div
                            className={`h-full ${pct >= 100 ? 'bg-destructive' : pct >= 80 ? 'bg-yellow-500' : 'bg-primary'}`}
                            style={{ width: `${pct}%` }}
                          />
                        </div>
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
