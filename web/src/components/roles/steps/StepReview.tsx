import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle } from 'lucide-react';
import { Label } from '@/components/ui/label';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { policyToPerms, type PermissionDef } from '@/routes/admin/roles/types';

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
  /** Permission catalog — needed to parse policy JSON for the
   *  "what will actually be saved" preview in policy mode. */
  permissions: PermissionDef[];
  /** Permission keys flagged as dangerous in the catalog — surfaced
   *  with a warning in the review. */
  dangerousKeys: Set<string>;
}

/**
 * Wizard final step: read-only summary of the role about to be saved.
 * In policy mode the JSON is the source of truth — we re-parse it here
 * so the admin sees the *effective* perms / scope that will land in
 * the database, not just whatever the simple-mode form had on entry.
 */
export function StepReview({
  name,
  description,
  mode,
  perms,
  policyJson,
  models,
  mcpTools,
  permissions,
  dangerousKeys,
}: StepReviewProps) {
  const { t } = useTranslation();

  // In policy mode the JSON is authoritative. Parse it once for an
  // effective-payload preview so the admin sees what the backend will
  // actually receive after JSON → side-fields conversion.
  const effective = useMemo(() => {
    if (mode === 'simple') {
      return { perms, models, mcpTools, parseError: false };
    }
    const parsed = policyToPerms(policyJson, permissions);
    return {
      perms: parsed.perms,
      models: parsed.models,
      mcpTools: parsed.mcpTools,
      parseError: parsed.parseError,
    };
  }, [mode, perms, models, mcpTools, policyJson, permissions]);

  const dangerous = useMemo(
    () => Array.from(effective.perms).filter((k) => dangerousKeys.has(k)),
    [effective.perms, dangerousKeys],
  );

  return (
    <div className="space-y-4 text-sm">
      {effective.parseError && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertDescription>{t('roles.invalidJson')}</AlertDescription>
        </Alert>
      )}

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

      <div className="grid gap-3 md:grid-cols-3">
        <div>
          <Label className="text-xs text-muted-foreground">{t('roles.permissions')}</Label>
          <p className="font-mono tabular-nums">
            {t('roles.reviewPermsCount', { count: effective.perms.size })}
          </p>
        </div>
        <div>
          <Label className="text-xs text-muted-foreground">{t('roles.modelsLabel')}</Label>
          <p className="font-mono tabular-nums">
            {effective.models === null
              ? t('roles.unrestricted')
              : t('roles.reviewScopedCount', { count: effective.models.size })}
          </p>
        </div>
        <div>
          <Label className="text-xs text-muted-foreground">{t('roles.mcpToolsLabel')}</Label>
          <p className="font-mono tabular-nums">
            {effective.mcpTools === null
              ? t('roles.unrestricted')
              : t('roles.reviewScopedCount', { count: effective.mcpTools.size })}
          </p>
        </div>
      </div>

      {dangerous.length > 0 && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertDescription>
            <strong>{t('roles.dangerous')}:</strong>{' '}
            <code className="font-mono text-xs">{dangerous.join(', ')}</code>
          </AlertDescription>
        </Alert>
      )}

      {mode === 'policy' && policyJson && (
        <div>
          <Label className="text-xs text-muted-foreground">{t('roles.policyMode')}</Label>
          <pre className="mt-1 max-h-64 overflow-auto rounded border bg-muted/50 p-2 font-mono text-[10px]">
            {policyJson}
          </pre>
        </div>
      )}
    </div>
  );
}
