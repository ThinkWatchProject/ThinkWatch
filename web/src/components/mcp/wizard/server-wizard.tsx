import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
import { Card } from '@/components/ui/card';
import { apiPost, api } from '@/lib/api';
import { toast } from 'sonner';
import { useWizardState } from './use-wizard-state';
import { StepSource } from './step-source';
import { StepAuthShape } from './step-auth-shape';
import { StepCredentialOwner } from './step-credential-owner';
import { StepMetadata } from './step-metadata';

interface McpServerSummary {
  name: string;
  namespace_prefix: string;
}

/**
 * Top-level component for the new-server registration wizard. Lives
 * at /mcp/servers/new — a real route, not a dialog, so the OAuth
 * admin_shared dance can navigate to the upstream and come back via
 * `#wizard_resume={session_id}` URL fragment without losing wizard
 * progress.
 *
 * Step skipping: anonymous probes that succeeded jump from Step 1
 * straight to Step 4. Admin can still walk back to Step 2 / 3 to
 * override (e.g. add custom headers, force PAT-only).
 */
export function ServerWizardPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const wiz = useWizardState();
  const [taken, setTaken] = useState<{ names: Set<string>; prefixes: Set<string> }>({
    names: new Set(),
    prefixes: new Set(),
  });
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState('');

  // Load existing server names / prefixes for collision-free defaults.
  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const list = await api<McpServerSummary[]>('/api/mcp/servers');
        if (!alive) return;
        setTaken({
          names: new Set(list.map((s) => s.name)),
          prefixes: new Set(list.map((s) => s.namespace_prefix)),
        });
      } catch {
        /* ignore — collision guard is best-effort */
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const goNext = () => {
    const cur = wiz.state.step;
    // Anonymous shortcut: skip Step 2 and Step 3 if Step 1 detected
    // a public server. Admin can still go back and pick a non-anon
    // shape to override.
    if (cur === 1 && wiz.state.probe?.anonymous_ok && wiz.state.auth_shape === 'anonymous') {
      wiz.goToStep(4);
      return;
    }
    if (cur === 2 && wiz.state.auth_shape === 'anonymous') {
      // No credential to own when there's no auth.
      wiz.patch({ credential_owner: 'per_user', shared_pending: null });
      wiz.goToStep(4);
      return;
    }
    if (cur < 4) {
      wiz.goToStep((cur + 1) as 1 | 2 | 3 | 4);
    }
  };

  const goBack = () => {
    const cur = wiz.state.step;
    // Mirror the skip logic in reverse.
    if (cur === 4 && wiz.state.auth_shape === 'anonymous') {
      wiz.goToStep(1);
      return;
    }
    if (cur > 1) {
      wiz.goToStep((cur - 1) as 1 | 2 | 3 | 4);
    }
  };

  const cancel = async () => {
    await wiz.reset();
    navigate({ to: '/mcp/servers' });
  };

  const submit = async () => {
    setSubmitError('');
    setSubmitting(true);
    try {
      const oauthFields =
        wiz.state.auth_shape === 'oauth'
          ? {
              oauth_issuer: wiz.state.oauth.issuer || null,
              oauth_authorization_endpoint:
                wiz.state.oauth.authorization_endpoint || null,
              oauth_token_endpoint: wiz.state.oauth.token_endpoint || null,
              oauth_revocation_endpoint:
                wiz.state.oauth.revocation_endpoint || null,
              oauth_userinfo_endpoint:
                wiz.state.oauth.userinfo_endpoint || null,
              oauth_client_id: wiz.state.oauth.client_id || null,
              oauth_client_secret: wiz.state.oauth.client_secret || null,
              oauth_scopes: wiz.state.oauth.scopes
                .split(/\s+/)
                .map((s) => s.trim())
                .filter(Boolean),
            }
          : {};

      const isStatic = wiz.state.auth_shape === 'static';

      const customHeadersObj =
        wiz.state.custom_headers.length > 0
          ? Object.fromEntries(wiz.state.custom_headers.filter(([k]) => k.trim()))
          : null;

      // Inline shared credential delivery — three mutually exclusive
      // paths driven by the wizard's Step 3 state:
      //   - shared_pending.kind === 'oauth_done' ⇒ wizard_session_id
      //     tells the backend to GETDEL the Redis blob.
      //   - shared_pending.kind === 'static_paste' ⇒ shared_static_token
      //     ships the plaintext token (encrypted at rest by the
      //     handler before insert).
      //   - per_user / no shared cred ⇒ neither field set.
      const sharedFields =
        wiz.state.credential_owner === 'admin_shared'
          ? wiz.state.shared_pending?.kind === 'oauth_done'
            ? { wizard_session_id: wiz.state.wizard_session_id }
            : wiz.state.shared_pending?.kind === 'static_paste'
              ? { shared_static_token: wiz.state.shared_pending.token }
              : {}
          : {};

      await apiPost('/api/mcp/servers', {
        name: wiz.state.name.trim(),
        namespace_prefix: wiz.state.namespace_prefix || undefined,
        display_label: wiz.state.display_label.trim() || null,
        description: wiz.state.description || null,
        endpoint_url: wiz.state.endpoint_url.trim(),
        transport_type: wiz.state.transport_type,
        ...oauthFields,
        auth_shape: wiz.state.auth_shape,
        static_token_help_url: isStatic
          ? wiz.state.static_token_help_url || null
          : null,
        custom_headers: customHeadersObj,
        cache_ttl_secs: wiz.state.cache_ttl_secs
          ? Number(wiz.state.cache_ttl_secs)
          : undefined,
        auth_header_name: wiz.state.auth_header_name,
        auth_value_template: wiz.state.auth_value_template,
        credential_owner: wiz.state.credential_owner,
        ...sharedFields,
      });

      // Different success message depending on what's still left for
      // the admin to do.
      if (wiz.state.auth_shape === 'anonymous') {
        toast.success(t('mcpServers.wizard.savedReady'));
      } else if (wiz.state.credential_owner === 'admin_shared') {
        toast.success(t('mcpServers.wizard.savedSharedReady'));
      } else {
        toast.success(t('mcpServers.wizard.savedNextConnections'), {
          duration: 8000,
          action: {
            label: t('mcpStore.goToConnections'),
            onClick: () => {
              window.location.href = '/connections';
            },
          },
        });
      }

      // Wipe sessionStorage; navigate back to the list.
      await wiz.reset();
      navigate({ to: '/mcp/servers' });
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : 'Failed to register server');
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="space-y-4 p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">
          {t('mcpServers.wizard.pageTitle')}
        </h1>
        <p className="text-sm text-muted-foreground">
          {t('mcpServers.wizard.pageHint')}
        </p>
      </div>

      <StepIndicator step={wiz.state.step} authShape={wiz.state.auth_shape} />

      <Card className="p-4">
        {wiz.state.step === 1 && (
          <StepSource
            state={wiz.state}
            patch={wiz.patch}
            patchOAuth={wiz.patchOAuth}
            onNext={goNext}
            onCancel={cancel}
          />
        )}
        {wiz.state.step === 2 && (
          <StepAuthShape
            state={wiz.state}
            patch={wiz.patch}
            patchOAuth={wiz.patchOAuth}
            onNext={goNext}
            onBack={goBack}
          />
        )}
        {wiz.state.step === 3 && (
          <StepCredentialOwner
            state={wiz.state}
            patch={wiz.patch}
            onNext={goNext}
            onBack={goBack}
            resumeChecking={wiz.resumeChecking}
          />
        )}
        {wiz.state.step === 4 && (
          <StepMetadata
            state={wiz.state}
            patch={wiz.patch}
            taken={taken}
            onBack={goBack}
            onSubmit={submit}
            submitting={submitting}
            submitError={submitError}
          />
        )}
      </Card>
    </div>
  );
}

/** Step indicator at the top of the wizard. Anonymous-flow paths
 *  visually skip Step 2 + Step 3. */
function StepIndicator({
  step,
  authShape,
}: {
  step: 1 | 2 | 3 | 4;
  authShape: string;
}) {
  const { t } = useTranslation();
  const skipAuth = authShape === 'anonymous';
  const labels: Array<{ n: 1 | 2 | 3 | 4; label: string; skipped?: boolean }> = [
    { n: 1, label: t('mcpServers.wizard.stepLabels.source') },
    { n: 2, label: t('mcpServers.wizard.stepLabels.auth'), skipped: skipAuth },
    { n: 3, label: t('mcpServers.wizard.stepLabels.owner'), skipped: skipAuth },
    { n: 4, label: t('mcpServers.wizard.stepLabels.metadata') },
  ];
  return (
    <div className="flex items-center gap-2 overflow-x-auto text-xs">
      {labels.map((l, i) => (
        <div key={l.n} className="flex items-center gap-2">
          {i > 0 && <span className="text-muted-foreground">→</span>}
          <span
            className={
              l.n === step
                ? 'rounded bg-primary/10 px-2 py-0.5 font-medium text-primary'
                : l.skipped
                  ? 'text-muted-foreground/50 line-through'
                  : 'text-muted-foreground'
            }
          >
            {l.n}. {l.label}
          </span>
        </div>
      ))}
    </div>
  );
}
