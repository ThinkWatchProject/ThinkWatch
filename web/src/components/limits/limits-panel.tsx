// ============================================================================
// Generic limits + budgets editor
//
// Inline editor for the role surface_constraints object — sliding-window
// rate-limit rules (1m / 5m / 1h / 5h / 1d / 1w) plus natural-period
// budget caps (daily / weekly / monthly) in weighted tokens. Roles
// carry their rules / budgets inline on the row (JSONB column) rather
// than in the `rate_limit_rules` / `budget_caps` side tables, so this
// component edits a plain in-memory object and the parent role-edit
// flow ships it back as part of the role PATCH body.
//
// Caller responsibilities:
//   - surfaces: which gateways apply to this role (`ai_gateway`,
//     `mcp_gateway`, or both). The rules table only renders / lets
//     the admin add rules for these surfaces.
//   - allowBudgets: whether to render the caps section. mcp-only
//     roles pass false.
//   - value / onChange: the parsed surface_constraints object.
// ============================================================================

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';

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

export type Surface = 'ai_gateway' | 'mcp_gateway';
type Metric = 'requests' | 'tokens';
type Period = 'daily' | 'weekly' | 'monthly';

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

type TFn = (key: string, opts?: Record<string, unknown>) => string;

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
  /// Which gateway surfaces are valid for this role. The rules table
  /// only shows / lets the admin add rules for these surfaces.
  surfaces: Surface[];
  /// Whether to render the budget caps section.
  allowBudgets: boolean;
  /// In-memory constraints object — the parent role form owns it and
  /// ships the updated value as part of the role PATCH body.
  value: ParsedConstraints;
  onChange: (next: ParsedConstraints) => void;
  /// Strip the bordered card and render as a popover-triggering chip
  /// so the panel embeds inline under a permission group without a
  /// nested-card visual.
  compact?: boolean;
}

export function LimitsPanel({
  surfaces,
  allowBudgets,
  value,
  onChange,
  compact,
}: LimitsPanelProps) {
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

  const body = (
    <div className="space-y-4">
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
                    <td className="px-2 py-1.5 font-mono text-[10px]">
                      {t(`limits.metric_${rule.metric}` as const)}
                    </td>
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

  if (compact) {
    const totalRules = allRules.length;
    const totalBudgets = allBudgets.length;
    const summaryText =
      totalRules === 0 && totalBudgets === 0
        ? t('roles.unrestricted')
        : allowBudgets
          ? t('limits.summary', { rules: totalRules, caps: totalBudgets })
          : t('limits.summaryRulesOnly', { rules: totalRules });
    return (
      <>
        <span className="text-muted-foreground">{t('limits.title')}:</span>
        <Popover modal>
          <PopoverTrigger asChild>
            <Button type="button" variant="outline" size="sm" className="h-5 gap-1 px-2 text-xs">
              {summaryText}
              <ChevronDown className="h-3 w-3 opacity-60" />
            </Button>
          </PopoverTrigger>
          <PopoverContent
            className="w-[42rem] overflow-y-auto p-3"
            style={{ maxHeight: 'var(--radix-popover-content-available-height)' }}
            align="start"
            collisionPadding={16}
          >
            {body}
          </PopoverContent>
        </Popover>
      </>
    );
  }

  return <div className="space-y-4 rounded-md border bg-muted/20 px-3 py-2">{body}</div>;
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
    <div className="flex flex-wrap items-end gap-1.5 rounded-md border bg-muted/10 p-1.5">
      {surfaces.length > 1 && (
        <div className="space-y-0.5">
          <Label className="text-[10px] text-muted-foreground">{t('limits.surface')}</Label>
          <Select value={surface} onValueChange={(v) => setSurfaceWithMetric(v as Surface)}>
            <SelectTrigger className="w-32 text-xs" style={{ height: 28 }}>
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
      <div className="space-y-0.5">
        <Label className="text-[10px] text-muted-foreground">{t('limits.metric')}</Label>
        <Select
          value={metric}
          onValueChange={(v) => setMetric(v as Metric)}
          disabled={surface === 'mcp_gateway'}
        >
          <SelectTrigger className="w-28 text-xs" style={{ height: 28 }}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="requests">{t('limits.metric_requests')}</SelectItem>
            <SelectItem value="tokens">{t('limits.metric_tokens')}</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-0.5">
        <Label className="text-[10px] text-muted-foreground">{t('limits.window')}</Label>
        <Select value={windowKey} onValueChange={setWindowKey}>
          <SelectTrigger className="w-24 text-xs" style={{ height: 28 }}>
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
      <div className="space-y-0.5">
        <Label className="text-[10px] text-muted-foreground">{t('limits.maxCount')}</Label>
        <Input
          type="number"
          min={1}
          value={maxCount}
          onChange={(e) => setMaxCount(e.target.value)}
          placeholder={metric === 'tokens' ? '100000' : '60'}
          className="w-28 text-xs"
          style={{ height: 28 }}
        />
      </div>
      <Button type="button" onClick={submit} disabled={!maxCount} style={{ height: 28 }}>
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
    <div className="flex flex-wrap items-end gap-1.5 rounded-md border bg-muted/10 p-1.5">
      {surfaces.length > 1 && (
        <div className="space-y-0.5">
          <Label className="text-[10px] text-muted-foreground">{t('limits.surface')}</Label>
          <Select value={surface} onValueChange={(v) => setSurface(v as Surface)}>
            <SelectTrigger className="w-32 text-xs" style={{ height: 28 }}>
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
      <div className="space-y-0.5">
        <Label className="text-[10px] text-muted-foreground">{t('limits.period')}</Label>
        <Select value={period} onValueChange={(v) => setPeriod(v as Period)}>
          <SelectTrigger className="w-28 text-xs" style={{ height: 28 }}>
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
      <div className="space-y-0.5">
        <Label className="text-[10px] text-muted-foreground">{t('limits.limitTokens')}</Label>
        <Input
          type="number"
          min={1}
          value={limitTokens}
          onChange={(e) => setLimitTokens(e.target.value)}
          placeholder="1000000"
          className="w-32 text-xs"
          style={{ height: 28 }}
        />
      </div>
      <Button type="button" onClick={submit} disabled={!limitTokens} style={{ height: 28 }}>
        <Plus className="mr-1 h-3 w-3" />
        {t('limits.add')}
      </Button>
    </div>
  );
}
