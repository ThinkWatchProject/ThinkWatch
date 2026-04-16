import { useTranslation } from 'react-i18next';
import { Checkbox } from '@/components/ui/checkbox';

export type DraftSurface = 'ai_gateway' | 'mcp_gateway';

/**
 * In-memory rate-limit / budget drafts. Each preset is a coarse-grain
 * starter so a freshly-created role can have meaningful guard-rails on
 * day one without forcing the admin to navigate into the LimitsPanel.
 */
export interface LimitDraft {
  kind: 'rule' | 'budget';
  surface: DraftSurface;
  /** Rule-only: which counter to enforce. */
  metric?: 'requests' | 'tokens';
  /** Rule-only: window in seconds. */
  window_secs?: number;
  /** Rule-only: max events per window. */
  max_count?: number;
  /** Budget-only: token ceiling per period. */
  limit_tokens?: number;
  /** Budget-only: rolling period. */
  period?: 'daily' | 'weekly' | 'monthly';
}

// Budget presets carry a token ceiling rather than a dollar figure —
// the backend `SurfaceBudget` shape is token-denominated (matches the
// existing `budget_caps.limit_tokens` column). The rough conversion
// used here (50 / 1000 USD → 1M / 20M tokens at ~$50 / 1M tokens on a
// premium model) is deliberately conservative.
export const PRESET_DRAFTS: { id: string; labelKey: string; draft: LimitDraft }[] = [
  {
    id: 'ai_100_rpm',
    labelKey: 'roles.preset_ai_100_rpm',
    draft: { kind: 'rule', surface: 'ai_gateway', metric: 'requests', window_secs: 60, max_count: 100 },
  },
  {
    id: 'ai_50k_tpm',
    labelKey: 'roles.preset_ai_50k_tpm',
    draft: { kind: 'rule', surface: 'ai_gateway', metric: 'tokens', window_secs: 60, max_count: 50000 },
  },
  {
    id: 'mcp_60_rpm',
    labelKey: 'roles.preset_mcp_60_rpm',
    draft: { kind: 'rule', surface: 'mcp_gateway', metric: 'requests', window_secs: 60, max_count: 60 },
  },
  {
    id: 'budget_1m_daily',
    labelKey: 'roles.preset_budget_1m_daily',
    draft: { kind: 'budget', surface: 'ai_gateway', limit_tokens: 1_000_000, period: 'daily' },
  },
  {
    id: 'budget_20m_monthly',
    labelKey: 'roles.preset_budget_20m_monthly',
    draft: { kind: 'budget', surface: 'ai_gateway', limit_tokens: 20_000_000, period: 'monthly' },
  },
];

interface LimitsDraftEditorProps {
  /** Set of preset ids the admin has selected. */
  selected: Set<string>;
  onChange: (next: Set<string>) => void;
  /** Restrict visible presets to one surface. Omit to show all. */
  surface?: DraftSurface;
}

/**
 * Tiny preset-chip picker shown only in the CREATE wizard inline under
 * each surface's permission group. Real fine-grained editing happens
 * in LimitsPanel after the role exists.
 */
export function LimitsDraftEditor({ selected, onChange, surface }: LimitsDraftEditorProps) {
  const { t } = useTranslation();
  const visible = surface
    ? PRESET_DRAFTS.filter((p) => p.draft.surface === surface)
    : PRESET_DRAFTS;
  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    onChange(next);
  };
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">{t('roles.limitsDraftHint')}</p>
      <div className="grid gap-2 sm:grid-cols-2">
        {visible.map((p) => (
          <label
            key={p.id}
            className="flex cursor-pointer items-center gap-2 rounded-md border bg-muted/20 px-3 py-2 text-xs hover:bg-muted/30"
          >
            <Checkbox checked={selected.has(p.id)} onCheckedChange={() => toggle(p.id)} />
            <span>{t(p.labelKey)}</span>
          </label>
        ))}
      </div>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Surface-constraints shape + draft folding
// ----------------------------------------------------------------------------

export interface SurfaceRule {
  metric: 'requests' | 'tokens';
  window_secs: number;
  max_count: number;
  enabled: boolean;
}

export interface SurfaceBudget {
  period: 'daily' | 'weekly' | 'monthly';
  limit_tokens: number;
  enabled: boolean;
}

export interface SurfaceBlock {
  rules: SurfaceRule[];
  budgets: SurfaceBudget[];
}

export interface SurfaceConstraints {
  ai_gateway?: SurfaceBlock;
  mcp_gateway?: SurfaceBlock;
}

/// Fold selected preset ids into a SurfaceConstraints object the role
/// POST endpoint accepts verbatim. Absent surfaces are omitted rather
/// than set to empty blocks — backend treats missing and empty as
/// identical and this keeps diffs readable.
export function draftsToSurfaceConstraints(selected: Set<string>): SurfaceConstraints {
  const out: SurfaceConstraints = {};
  const ensure = (s: DraftSurface): SurfaceBlock => {
    if (!out[s]) out[s] = { rules: [], budgets: [] };
    return out[s]!;
  };
  for (const id of selected) {
    const preset = PRESET_DRAFTS.find((p) => p.id === id);
    if (!preset) continue;
    const d = preset.draft;
    const block = ensure(d.surface);
    if (d.kind === 'rule' && d.metric && d.window_secs != null && d.max_count != null) {
      block.rules.push({
        metric: d.metric,
        window_secs: d.window_secs,
        max_count: d.max_count,
        enabled: true,
      });
    } else if (d.kind === 'budget' && d.period && d.limit_tokens != null) {
      block.budgets.push({
        period: d.period,
        limit_tokens: d.limit_tokens,
        enabled: true,
      });
    }
  }
  return out;
}
