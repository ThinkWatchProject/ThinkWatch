import { useTranslation } from 'react-i18next';
import { Label } from '@/components/ui/label';

interface StepReviewProps {
  name: string;
  description: string;
  mode: 'simple' | 'policy';
  perms: Set<string>;
  policyJson: string;
  /** `null` = unrestricted. */
  models: Set<string> | null;
  /** `null` = unrestricted. */
  mcpTools: Set<string> | null;
}

/**
 * Wizard final step: read-only summary of the role about to be saved.
 * Shows basics, permission count, scope constraints, and the raw policy
 * JSON when policy mode is active.
 */
export function StepReview({
  name,
  description,
  mode,
  perms,
  policyJson,
  models,
  mcpTools,
}: StepReviewProps) {
  const { t } = useTranslation();
  return (
    <div className="space-y-4 text-sm">
      <div className="grid gap-3 md:grid-cols-2">
        <div>
          <Label className="text-xs text-muted-foreground">{t('common.name')}</Label>
          <p className="font-mono">{name || <span className="text-destructive">—</span>}</p>
        </div>
        <div>
          <Label className="text-xs text-muted-foreground">{t('common.description')}</Label>
          <p className="text-muted-foreground">{description || '—'}</p>
        </div>
      </div>
      <div>
        <Label className="text-xs text-muted-foreground">{t('roles.permissions')}</Label>
        <p>
          {mode === 'simple'
            ? t('roles.reviewPermsCount', { count: perms.size })
            : t('roles.reviewPolicyMode')}
        </p>
      </div>
      <div>
        <Label className="text-xs text-muted-foreground">{t('roles.modelsLabel')}</Label>
        <p>
          {models === null
            ? t('roles.unrestricted')
            : t('roles.reviewScopedCount', { count: models.size })}
        </p>
      </div>
      <div>
        <Label className="text-xs text-muted-foreground">{t('roles.mcpToolsLabel')}</Label>
        <p>
          {mcpTools === null
            ? t('roles.unrestricted')
            : t('roles.reviewScopedCount', { count: mcpTools.size })}
        </p>
      </div>
      {mode === 'policy' && policyJson && (
        <div>
          <Label className="text-xs text-muted-foreground">{t('roles.policyMode')}</Label>
          <pre className="mt-1 max-h-48 overflow-auto rounded border bg-muted/50 p-2 font-mono text-[10px]">
            {policyJson}
          </pre>
        </div>
      )}
    </div>
  );
}
