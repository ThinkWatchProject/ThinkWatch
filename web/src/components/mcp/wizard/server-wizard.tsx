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
    // Step 2 → Step 4 when admin chose anonymous (Step 3 is meaningless
    // — no credential to own). Step 1 used to short-circuit here too,
    // but that created a back→next loop: "Back" from Step 4 lands on
    // Step 1, then "Next" auto-skipped to Step 4 again, blocking the
    // admin from ever reaching Step 2. The anonymous shortcut from
    // Step 1 now lives inside `step-source.tsx`'s probe handler and
    // fires exactly once on probe completion, not on every traversal.
    if (cur === 2 && wiz.state.auth_shape === 'anonymous') {
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
    // From Step 4, mirror the forward anonymous shortcut: jump back
    // to Step 2 (where the admin can change the shape away from
    // anonymous). Going back to Step 1 would force a re-probe and
    // re-trigger the auto-skip, which is the loop we just fixed.
    if (cur === 4 && wiz.state.auth_shape === 'anonymous') {
      wiz.goToStep(2);
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
        // transport_type intentionally omitted — the wizard never
        // asks for it, the state default ('streamable_http') would
        // suppress the backend's auto-detect, and SSE-only upstreams
        // would then 4xx. Letting create_server detect from the
        // endpoint shape is correct.
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
        // When the wizard was opened from /mcp/store, ship the slug
        // back so the backend writes a `mcp_store_installs` audit row
        // and bumps `install_count` in the same TX as the server
        // INSERT. The same advisory lock + collision resolver as a
        // direct API install applies — `name` / `namespace_prefix`
        // get auto-suffixed if they collide.
        template_slug: wiz.state.template_slug || undefined,
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

      <StepIndicator
        step={wiz.state.step}
        authShape={wiz.state.auth_shape}
        onJump={wiz.goToStep}
      />

      <Card className="p-4">
        {wiz.state.step === 1 && (
          <StepSource
            state={wiz.state}
            patch={wiz.patch}
            patchOAuth={wiz.patchOAuth}
            onNext={goNext}
            goToStep={wiz.goToStep}
            onCancel={cancel}
            templateLoading={wiz.templateLoading}
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

/** Step indicator at the top of the wizard. Three chip states:
 *
 *   - `current`: the active step. Disabled (already here).
 *   - `skipped`: auto-skipped but the override still has semantic
 *     meaning (e.g. Step 2 / "Auth shape" when probe was anonymous —
 *     admin may want to flip to static / OAuth anyway). Clickable,
 *     dashed-underline + 60% opacity.
 *   - `disabled`: structurally meaningless given current state
 *     (e.g. Step 3 / "Credential" when auth_shape is anonymous —
 *     there's no credential to own). Not clickable, line-through. */
function StepIndicator({
  step,
  authShape,
  onJump,
}: {
  step: 1 | 2 | 3 | 4;
  authShape: string;
  onJump: (n: 1 | 2 | 3 | 4) => void;
}) {
  const { t } = useTranslation();
  const isAnon = authShape === 'anonymous';
  type ChipState = 'normal' | 'skipped' | 'disabled';
  const labels: Array<{ n: 1 | 2 | 3 | 4; label: string; chip: ChipState }> = [
    { n: 1, label: t('mcpServers.wizard.stepLabels.source'), chip: 'normal' },
    {
      n: 2,
      label: t('mcpServers.wizard.stepLabels.auth'),
      chip: isAnon ? 'skipped' : 'normal',
    },
    {
      n: 3,
      label: t('mcpServers.wizard.stepLabels.owner'),
      chip: isAnon ? 'disabled' : 'normal',
    },
    { n: 4, label: t('mcpServers.wizard.stepLabels.metadata'), chip: 'normal' },
  ];
  return (
    <div className="flex items-center gap-2 overflow-x-auto text-xs">
      {labels.map((l, i) => {
        const isCurrent = l.n === step;
        const isDisabled = l.chip === 'disabled' || isCurrent;
        const title = isCurrent
          ? undefined
          : l.chip === 'skipped'
            ? t('mcpServers.wizard.stepLabels.skippedHint')
            : l.chip === 'disabled'
              ? t('mcpServers.wizard.stepLabels.disabledHint')
              : undefined;
        return (
          <div key={l.n} className="flex items-center gap-2">
            {i > 0 && <span className="text-muted-foreground">→</span>}
            <button
              type="button"
              onClick={() => !isDisabled && onJump(l.n)}
              disabled={isDisabled}
              title={title}
              className={
                isCurrent
                  ? 'cursor-default rounded bg-primary/10 px-2 py-0.5 font-medium text-primary'
                  : l.chip === 'skipped'
                    ? 'rounded px-2 py-0.5 text-muted-foreground/60 underline decoration-dashed underline-offset-2 hover:text-foreground hover:decoration-solid'
                    : l.chip === 'disabled'
                      ? 'cursor-not-allowed rounded px-2 py-0.5 text-muted-foreground/40 line-through'
                      : 'rounded px-2 py-0.5 text-muted-foreground hover:bg-muted hover:text-foreground'
              }
            >
              {l.n}. {l.label}
            </button>
          </div>
        );
      })}
    </div>
  );
}
