import { useTranslation } from 'react-i18next';
import { Checkbox } from '@/components/ui/checkbox';

/**
 * In-memory rate-limit / budget drafts. Each preset is a coarse-grain
 * starter so a freshly-created role can have meaningful guard-rails on
 * day one without forcing the admin to navigate into the LimitsPanel.
 */
export interface LimitDraft {
  kind: 'rule' | 'budget';
  surface: 'ai_gateway' | 'mcp_gateway';
  /** Rule-only: which counter to enforce. */
  metric?: 'requests' | 'tokens';
  /** Rule-only: window in seconds. */
  window_secs?: number;
  /** Rule-only: max events per window. */
  max_count?: number;
  /** Budget-only: USD ceiling per period. */
  limit_usd?: number;
  /** Budget-only: rolling period. */
  period?: 'daily' | 'monthly';
}

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
    id: 'budget_50_daily',
    labelKey: 'roles.preset_budget_50_daily',
    draft: { kind: 'budget', surface: 'ai_gateway', limit_usd: 50, period: 'daily' },
  },
  {
    id: 'budget_1000_monthly',
    labelKey: 'roles.preset_budget_1000_monthly',
    draft: { kind: 'budget', surface: 'ai_gateway', limit_usd: 1000, period: 'monthly' },
  },
];

interface LimitsDraftEditorProps {
  /** Set of preset ids the admin has selected. */
  selected: Set<string>;
  onChange: (next: Set<string>) => void;
}

/**
 * Tiny preset-chip picker shown only in the CREATE wizard. Real fine-
 * grained editing happens in LimitsPanel after the role exists; this
 * just gets the most common policies in place during create so the
 * admin doesn't have to click "Save → Edit → Limits" every time.
 */
export function LimitsDraftEditor({ selected, onChange }: LimitsDraftEditorProps) {
  const { t } = useTranslation();
  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    onChange(next);
  };
  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">{t('roles.limitsDraftHint')}</p>
      <div className="grid gap-2 sm:grid-cols-2">
        {PRESET_DRAFTS.map((p) => (
          <label
            key={p.id}
            className="flex cursor-pointer items-center gap-2 rounded-md border bg-muted/20 px-3 py-2 text-sm hover:bg-muted/30"
          >
            <Checkbox checked={selected.has(p.id)} onCheckedChange={() => toggle(p.id)} />
            <span>{t(p.labelKey)}</span>
          </label>
        ))}
      </div>
      <p className="text-[10px] italic text-muted-foreground">
        {t('roles.limitsDraftFootnote')}
      </p>
    </div>
  );
}

/**
 * Persist selected preset drafts to the backend after a role is created.
 * Best-effort: failures surface via toast in the caller; the role is
 * already saved so we don't roll back.
 */
export async function persistDrafts(
  roleId: string,
  selected: Set<string>,
  apiPost: <T>(url: string, body: unknown) => Promise<T>,
): Promise<void> {
  for (const id of selected) {
    const preset = PRESET_DRAFTS.find((p) => p.id === id);
    if (!preset) continue;
    const d = preset.draft;
    if (d.kind === 'rule') {
      await apiPost(`/api/admin/limits/role/${roleId}/rules`, {
        surface: d.surface,
        metric: d.metric,
        window_secs: d.window_secs,
        max_count: d.max_count,
        enabled: true,
      });
    } else {
      await apiPost(`/api/admin/limits/role/${roleId}/budgets`, {
        surface: d.surface,
        limit_usd: d.limit_usd,
        period: d.period,
        enabled: true,
      });
    }
  }
}
