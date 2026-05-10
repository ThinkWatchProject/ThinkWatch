import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, Loader2, Sparkles } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiPost } from '@/lib/api';
import type { OAuthProbeResult, ProbeResult, WizardState } from './types';

interface Props {
  state: WizardState;
  patch: (partial: Partial<WizardState>) => void;
  patchOAuth: (partial: Partial<WizardState['oauth']>) => void;
  onNext: () => void;
  /** Direct jump used by the anonymous-OK probe shortcut to land on
   *  Step 4. We can't call `onNext` because it intentionally always
   *  goes to Step 2 — the auto-skip is a one-shot probe-completion
   *  decision, not a recurring traversal rule. */
  goToStep: (step: 1 | 2 | 3 | 4) => void;
  onCancel: () => void;
  /** True while the `?template=<slug>` prefill fetch is in flight. */
  templateLoading?: boolean;
}

/**
 * Step 1 — paste an MCP endpoint URL, hit "Detect", and let the
 * backend tell us what's there. The probe answers three questions in
 * one round-trip:
 *   1. Is the endpoint reachable as MCP at all? (anonymous tools/list)
 *   2. Does it require credentials?
 *   3. If yes, is the upstream advertising OAuth metadata (RFC 9728
 *      → 8414 → 7591 chain)?
 *
 * Each outcome routes the wizard differently:
 *   - reachable + no auth ⇒ skip Step 2 + Step 3 entirely
 *   - reachable + OAuth detected ⇒ Step 2 prefilled with discovered metadata
 *   - reachable + auth required, no OAuth ⇒ Step 2 with static-token default
 */
export function StepSource({
  state,
  patch,
  patchOAuth,
  onNext,
  goToStep,
  onCancel,
  templateLoading,
}: Props) {
  const { t } = useTranslation();
  const [probing, setProbing] = useState(false);
  const [error, setError] = useState('');

  /**
   * Probe the endpoint and, on success, advance to the next step.
   * "Probe" and "Next" used to be two buttons; collapsing them keeps
   * Step 1 to a single decision (URL → continue) and means the
   * probe result the wizard relies on always reflects the URL the
   * admin most recently typed.
   */
  const runProbeAndNext = async () => {
    if (!state.endpoint_url.trim()) {
      setError(t('mcpServers.wizard.errors.endpointRequired'));
      return;
    }
    setError('');
    setProbing(true);
    try {
      // Step A — anonymous tools/list probe (existing endpoint).
      const test = await apiPost<{
        success: boolean;
        requires_auth: boolean;
        message: string;
        latency_ms: number;
        tools_count?: number;
        tools?: { name: string; description?: string | null }[];
      }>('/api/mcp/servers/test', {
        endpoint_url: state.endpoint_url.trim(),
      });

      const probe: ProbeResult = {
        anonymous_ok: test.success && !test.requires_auth,
        requires_auth: test.requires_auth,
        transport_type: 'streamable_http',
        tools: test.tools ?? [],
        latency_ms: test.latency_ms,
        message: test.message,
      };

      let oauth_probe: OAuthProbeResult | null = null;
      // Step B — only if auth is required, run the OAuth metadata
      // chain. For anonymous-OK servers it's wasted work.
      if (test.requires_auth) {
        try {
          const probeResult = await apiPost<{
            issuer?: string | null;
            is_public_client: boolean;
            redirect_uri: string;
            authorization_endpoint?: string | null;
            token_endpoint?: string | null;
            revocation_endpoint?: string | null;
            userinfo_endpoint?: string | null;
            scopes_supported?: string[];
            client_id?: string | null;
            client_secret?: string | null;
          }>('/api/admin/mcp/oauth-probe', {
            endpoint_url: state.endpoint_url.trim(),
          });
          oauth_probe = {
            issuer: probeResult.issuer ?? null,
            public_client: probeResult.is_public_client,
            redirect_uri: probeResult.redirect_uri,
            authorization_endpoint: probeResult.authorization_endpoint ?? null,
            token_endpoint: probeResult.token_endpoint ?? null,
            revocation_endpoint: probeResult.revocation_endpoint ?? null,
            userinfo_endpoint: probeResult.userinfo_endpoint ?? null,
            default_scopes: probeResult.scopes_supported ?? [],
            client_id: probeResult.client_id ?? null,
            client_secret: probeResult.client_secret ?? null,
          };
        } catch {
          // OAuth probe failure is non-fatal — admin will pick
          // static-token in Step 2.
        }
      }

      // Pre-populate step-2 defaults from the probe result so the
      // next step lands on a form that's already filled in.
      if (probe.anonymous_ok) {
        patch({ probe, oauth_probe, auth_shape: 'anonymous' });
      } else if (oauth_probe?.issuer) {
        patch({
          probe,
          oauth_probe,
          auth_shape: 'oauth',
        });
        patchOAuth({
          issuer: oauth_probe.issuer ?? '',
          authorization_endpoint: oauth_probe.authorization_endpoint ?? '',
          token_endpoint: oauth_probe.token_endpoint ?? '',
          revocation_endpoint: oauth_probe.revocation_endpoint ?? '',
          userinfo_endpoint: oauth_probe.userinfo_endpoint ?? '',
          client_id: oauth_probe.client_id ?? '',
          client_secret: oauth_probe.client_secret ?? '',
          scopes: (oauth_probe.default_scopes ?? []).join(' '),
        });
      } else {
        patch({ probe, oauth_probe, auth_shape: 'static' });
      }

      // Probe-failed-to-reach is the only case we keep the admin on
      // Step 1 — surface the message and let them fix the URL.
      // 401/403 (`requires_auth`) is *not* a failure here: it's the
      // expected response from auth-gated MCPs and the wizard treats
      // it as a successful probe with the static / OAuth path
      // pre-selected.
      if (!probe.anonymous_ok && !probe.requires_auth) {
        setError(probe.message);
        return;
      }
      // Anonymous shortcut: probe passed without credentials → no
      // auth shape to pick, no credential owner to choose. Jump to
      // Step 4 (confirm) directly. This skip is INTENTIONALLY a
      // one-shot probe-completion decision, not a permanent traversal
      // rule — once the admin lands on Step 4 they can `Back` through
      // every step normally to adjust shape / credentials.
      if (probe.anonymous_ok) {
        goToStep(4);
        return;
      }
      onNext();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Probe failed');
    } finally {
      setProbing(false);
    }
  };

  return (
    <div className="space-y-4">
      {state.template_slug && state.template_name && (
        <Alert>
          <Sparkles className="h-4 w-4" />
          <AlertDescription>
            {t('mcpServers.wizard.templatePrefillBanner', {
              name: state.template_name,
            })}
          </AlertDescription>
        </Alert>
      )}

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="space-y-2">
        <Label htmlFor="wizard-endpoint">{t('mcpServers.endpointUrl')}</Label>
        <Input
          id="wizard-endpoint"
          value={state.endpoint_url}
          onChange={(e) =>
            patch({ endpoint_url: e.target.value, probe: null, oauth_probe: null })
          }
          placeholder="https://example.com/mcp"
          autoFocus
          disabled={templateLoading}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              void runProbeAndNext();
            }
          }}
        />
        <p className="text-xs text-muted-foreground">{t('mcpServers.wizard.urlHint')}</p>
      </div>

      <div className="flex items-center justify-between gap-2 pt-2">
        <Button variant="outline" type="button" onClick={onCancel}>
          {t('common.cancel')}
        </Button>
        <Button
          type="button"
          onClick={runProbeAndNext}
          disabled={probing || templateLoading || !state.endpoint_url.trim()}
        >
          {(probing || templateLoading) && (
            <Loader2 className="h-4 w-4 animate-spin" />
          )}
          {probing
            ? t('mcpServers.wizard.probing')
            : templateLoading
              ? t('mcpServers.wizard.loadingTemplate')
              : t('common.next')}
        </Button>
      </div>
    </div>
  );
}
