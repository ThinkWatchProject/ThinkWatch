import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  AlertCircle,
  ArrowLeft,
  CheckCircle2,
  Loader2,
  ShieldCheck,
  Users,
} from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiPost } from '@/lib/api';
import { cn } from '@/lib/utils';
import type { CredentialOwner, WizardState } from './types';

interface Props {
  state: WizardState;
  patch: (partial: Partial<WizardState>) => void;
  onNext: () => void;
  onBack: () => void;
  resumeChecking: boolean;
}

/**
 * Step 3 — choose who supplies the credential. Two cards (not radios)
 * because the choice is structurally weighty: per_user vs admin_shared
 * has different audit semantics, recovery story, and blast radius
 * when a token leaks.
 *
 * For admin_shared, the credential entry happens INLINE here. OAuth
 * triggers a real cross-origin redirect to the upstream's authorize
 * page; the wizard's sessionStorage state lets us land back on this
 * step with the credential pre-staged in Redis (see useWizardState's
 * resume logic).
 *
 * Skipped entirely when `auth_shape='anonymous'` — there's no
 * credential to own.
 */
export function StepCredentialOwner({
  state,
  patch,
  onNext,
  onBack,
  resumeChecking,
}: Props) {
  const { t } = useTranslation();
  const [authorizing, setAuthorizing] = useState(false);
  const [pasted, setPasted] = useState(
    state.shared_pending?.kind === 'static_paste' ? state.shared_pending.token : '',
  );
  const [error, setError] = useState('');

  // Derive what the chosen owner needs to "be ready" for Step 4.
  const ownerReady =
    state.credential_owner === 'per_user' ||
    (state.credential_owner === 'admin_shared' &&
      (state.shared_pending !== null || pasted.trim().length > 0));

  const setOwner = (owner: CredentialOwner) => {
    patch({ credential_owner: owner });
    if (owner === 'per_user') {
      // Drop any pending shared cred so we don't accidentally send
      // it to the backend on save.
      patch({ shared_pending: null });
      setPasted('');
    }
  };

  const startOAuthAuthorize = async () => {
    if (!state.oauth.token_endpoint || !state.oauth.client_id) {
      setError(t('mcpServers.wizard.shared.oauthFieldsMissing'));
      return;
    }
    setError('');
    setAuthorizing(true);
    try {
      const res = await apiPost<{ authorize_url: string }>(
        '/api/admin/mcp/oauth-wizard-authorize',
        {
          wizard_session_id: state.wizard_session_id,
          oauth_authorization_endpoint:
            state.oauth.authorization_endpoint || state.oauth_probe?.authorization_endpoint || '',
          oauth_token_endpoint: state.oauth.token_endpoint,
          oauth_client_id: state.oauth.client_id,
          oauth_client_secret: state.oauth.client_secret || null,
          oauth_scopes: state.oauth.scopes
            .split(/\s+/)
            .map((s) => s.trim())
            .filter(Boolean),
          oauth_userinfo_endpoint: state.oauth.userinfo_endpoint || null,
        },
      );
      // Hard navigate — the upstream redirect cycle dumps us back at
      // /mcp/servers/new#wizard_resume={session_id}, where the
      // controller hook restores everything.
      window.location.href = res.authorize_url;
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
      setAuthorizing(false);
    }
  };

  const onPasteChange = (v: string) => {
    setPasted(v);
    // Mirror to wizard state so going forward to Step 4 carries it.
    // The plaintext value is *not* persisted to sessionStorage —
    // see useWizardState.writeStorage.
    patch({
      shared_pending: v.trim().length > 0 ? { kind: 'static_paste', token: v } : null,
    });
  };

  const allowOAuth = state.auth_shape === 'oauth';
  const allowStatic = state.auth_shape === 'static';

  return (
    <div className="space-y-4">
      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="grid gap-3 sm:grid-cols-2">
        <OwnerCard
          icon={<Users className="h-5 w-5" />}
          title={t('mcpServers.wizard.owner.perUserTitle')}
          hint={t('mcpServers.wizard.owner.perUserHint')}
          pros={[
            t('mcpServers.wizard.owner.perUserPro1'),
            t('mcpServers.wizard.owner.perUserPro2'),
          ]}
          selected={state.credential_owner === 'per_user'}
          onSelect={() => setOwner('per_user')}
        />
        <OwnerCard
          icon={<ShieldCheck className="h-5 w-5" />}
          title={t('mcpServers.wizard.owner.sharedTitle')}
          hint={t('mcpServers.wizard.owner.sharedHint')}
          pros={[
            t('mcpServers.wizard.owner.sharedPro1'),
            t('mcpServers.wizard.owner.sharedCon1'),
          ]}
          selected={state.credential_owner === 'admin_shared'}
          onSelect={() => setOwner('admin_shared')}
        />
      </div>

      {state.credential_owner === 'admin_shared' && (
        <div className="rounded-md border p-3 space-y-3">
          {resumeChecking && (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              {t('mcpServers.wizard.shared.checkingResume')}
            </div>
          )}

          {!resumeChecking && state.shared_pending?.kind === 'oauth_done' && (
            <Alert className="border-emerald-500/30 bg-emerald-500/10 text-emerald-900 dark:text-emerald-200 [&_svg]:text-emerald-600 dark:[&_svg]:text-emerald-300">
              <CheckCircle2 className="h-4 w-4" />
              <AlertDescription className="space-y-0.5 text-xs">
                <p className="font-medium">{t('mcpServers.wizard.shared.oauthDone')}</p>
                {state.shared_pending.upstream_subject && (
                  <p>
                    {t('mcpServers.sharedCred.upstreamSubject')}:{' '}
                    <code className="font-mono">{state.shared_pending.upstream_subject}</code>
                  </p>
                )}
                {state.shared_pending.expires_at && (
                  <p>
                    {t('mcpServers.sharedCred.expiresAt')}:{' '}
                    {new Date(state.shared_pending.expires_at).toLocaleString()}
                  </p>
                )}
              </AlertDescription>
            </Alert>
          )}

          {!resumeChecking && state.shared_pending?.kind !== 'oauth_done' && (
            <>
              {allowOAuth && (
                <div className="space-y-2">
                  <Button
                    type="button"
                    onClick={startOAuthAuthorize}
                    disabled={authorizing}
                  >
                    {authorizing ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : null}
                    {t('mcpServers.wizard.shared.authorizeOAuthBtn')}
                  </Button>
                  <p className="text-xs text-muted-foreground">
                    {t('mcpServers.wizard.shared.authorizeOAuthHint')}
                  </p>
                </div>
              )}

              {allowStatic && (
                <div className="space-y-1.5">
                  <Label htmlFor="wizard-shared-token" className="text-xs">
                    {t('mcpServers.wizard.shared.pasteTokenLabel')}
                  </Label>
                  <Input
                    id="wizard-shared-token"
                    type="password"
                    autoComplete="off"
                    value={pasted}
                    onChange={(e) => onPasteChange(e.target.value)}
                    placeholder={t('mcpServers.wizard.shared.pastePlaceholder')}
                  />
                  {pasted.length > 0 && (
                    <div className="rounded border bg-muted/30 px-2 py-1 text-[11px] text-muted-foreground">
                      {t('mcpServers.authHeader.previewLabel')}:{' '}
                      <code className="font-mono">
                        {state.auth_header_name}:{' '}
                        {state.auth_value_template.replaceAll(
                          '{{token}}',
                          `${pasted.slice(0, 6)}…`,
                        )}
                      </code>
                    </div>
                  )}
                </div>
              )}
            </>
          )}
        </div>
      )}

      <div className="flex items-center justify-between gap-2 pt-2">
        <Button variant="outline" type="button" onClick={onBack}>
          <ArrowLeft className="h-4 w-4" />
          {t('common.back')}
        </Button>
        <Button type="button" onClick={onNext} disabled={!ownerReady}>
          {t('common.next')}
        </Button>
      </div>

      {!ownerReady && state.credential_owner === 'admin_shared' && (
        <p className="text-right text-xs text-muted-foreground">
          {t('mcpServers.wizard.shared.needCredentialBeforeNext')}
        </p>
      )}
    </div>
  );
}

function OwnerCard({
  icon,
  title,
  hint,
  pros,
  selected,
  onSelect,
}: {
  icon: React.ReactNode;
  title: string;
  hint: string;
  pros: string[];
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        'flex h-full flex-col gap-2 rounded-md border p-3 text-left transition-colors',
        selected
          ? 'border-primary bg-primary/5 ring-1 ring-primary'
          : 'hover:bg-muted/50',
      )}
    >
      <div className="flex items-center gap-2">
        <span className={cn('shrink-0', selected && 'text-primary')}>{icon}</span>
        <span className="text-sm font-medium">{title}</span>
        {selected && <CheckCircle2 className="ml-auto h-4 w-4 text-primary" />}
      </div>
      <p className="text-xs text-muted-foreground">{hint}</p>
      <ul className="mt-auto space-y-0.5 text-[11px] text-muted-foreground">
        {pros.map((p, i) => (
          <li key={i}>• {p}</li>
        ))}
      </ul>
    </button>
  );
}
