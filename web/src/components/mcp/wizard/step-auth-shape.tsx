import { useTranslation } from 'react-i18next';
import { ArrowLeft, Check, Info } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import {
  AuthHeaderFieldset,
  type AuthHeaderFields,
} from '@/components/mcp/auth-header-fieldset';
import { cn } from '@/lib/utils';
import type {
  AuthShape,
  OAuthProbeResult,
  ProbeResult,
  WizardState,
} from './types';

interface Props {
  state: WizardState;
  patch: (partial: Partial<WizardState>) => void;
  patchOAuth: (partial: Partial<WizardState['oauth']>) => void;
  onNext: () => void;
  onBack: () => void;
}

/**
 * Step 2 — pick the authentication shape.
 *
 * Anonymous probes that succeeded in Step 1 normally skip this step
 * entirely (the wizard shell jumps from Step 1 → Step 4); we render
 * the "anonymous" radio anyway for the admin who explicitly wants to
 * register a public service while overriding the probe's verdict.
 *
 * Header injection (`auth_header_name` + `auth_value_template`) is a
 * first-class field here, not buried in advanced. For X-API-Key
 * upstreams (Anthropic, Azure) it's the *only* setting that matters —
 * collapsing it into "advanced" is what made the old wizard fail
 * silently with the wrong header on save.
 */
export function StepAuthShape({ state, patch, patchOAuth, onNext, onBack }: Props) {
  const { t } = useTranslation();

  const setShape = (shape: AuthShape) => {
    patch({ auth_shape: shape });
  };

  const setHeader = (h: AuthHeaderFields) => {
    patch({ auth_header_name: h.headerName, auth_value_template: h.valueTemplate });
  };

  const showOAuth = state.auth_shape === 'oauth';
  const showStatic = state.auth_shape === 'static';

  // The "Next" button gates on the chosen shape having its required
  // fields filled. Anonymous needs nothing; OAuth needs issuer +
  // client_id; static needs auth_header_name + auth_value_template
  // (both have safe defaults so they're rarely empty in practice).
  const canProceed =
    state.auth_shape === 'anonymous'
      ? true
      : showOAuth
        ? !!state.oauth.issuer.trim() && !!state.oauth.client_id.trim()
        : showStatic
          ? !!state.auth_header_name.trim() && !!state.auth_value_template.trim()
          : false;

  return (
    <div className="space-y-4">
      {state.probe && (
        <ProbeSummary
          url={state.endpoint_url}
          probe={state.probe}
          oauthProbe={state.oauth_probe}
        />
      )}
      <RadioGroup
        value={state.auth_shape}
        onValueChange={(v) => setShape(v as AuthShape)}
        className="space-y-2"
      >
        <ShapeRow
          id="anonymous"
          value="anonymous"
          label={t('mcpServers.wizard.shape.anonymousTitle')}
          hint={t('mcpServers.wizard.shape.anonymousHint')}
          selected={state.auth_shape === 'anonymous'}
        />
        <ShapeRow
          id="oauth"
          value="oauth"
          label={t('mcpServers.wizard.shape.oauthTitle')}
          hint={t('mcpServers.wizard.shape.oauthHint')}
          selected={state.auth_shape === 'oauth'}
        />
        <ShapeRow
          id="static"
          value="static"
          label={t('mcpServers.wizard.shape.staticTitle')}
          hint={t('mcpServers.wizard.shape.staticHint')}
          selected={state.auth_shape === 'static'}
        />
      </RadioGroup>

      {showOAuth && (
        <div className="space-y-3 rounded-md border p-3">
          <p className="text-sm font-medium">{t('mcpServers.wizard.shape.oauthSection')}</p>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field
              id="oauth-issuer"
              label={t('mcpServers.oauth.issuer')}
              value={state.oauth.issuer}
              onChange={(v) => patchOAuth({ issuer: v })}
              required
            />
            <Field
              id="oauth-client-id"
              label={t('mcpServers.oauth.clientId')}
              value={state.oauth.client_id}
              onChange={(v) => patchOAuth({ client_id: v })}
              required
            />
            <Field
              id="oauth-client-secret"
              label={t('mcpServers.oauth.clientSecret')}
              value={state.oauth.client_secret}
              onChange={(v) => patchOAuth({ client_secret: v })}
              type="password"
              hint={
                state.oauth_probe?.public_client
                  ? t('mcpServers.oauth.publicClientNote')
                  : undefined
              }
            />
            <Field
              id="oauth-scopes"
              label={t('mcpServers.oauth.scopes')}
              value={state.oauth.scopes}
              onChange={(v) => patchOAuth({ scopes: v })}
              placeholder="read write"
            />
          </div>
          <details className="text-xs">
            <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
              {t('mcpServers.oauth.advanced')}
            </summary>
            <div className="mt-2 grid gap-3 sm:grid-cols-2">
              <Field
                id="oauth-auth-endpoint"
                label={t('mcpServers.oauth.authEndpoint')}
                value={state.oauth.authorization_endpoint}
                onChange={(v) => patchOAuth({ authorization_endpoint: v })}
              />
              <Field
                id="oauth-token-endpoint"
                label={t('mcpServers.oauth.tokenEndpoint')}
                value={state.oauth.token_endpoint}
                onChange={(v) => patchOAuth({ token_endpoint: v })}
              />
              <Field
                id="oauth-revoke-endpoint"
                label={t('mcpServers.oauth.revocationEndpoint')}
                value={state.oauth.revocation_endpoint}
                onChange={(v) => patchOAuth({ revocation_endpoint: v })}
              />
              <Field
                id="oauth-userinfo-endpoint"
                label={t('mcpServers.oauth.userinfoEndpoint')}
                value={state.oauth.userinfo_endpoint}
                onChange={(v) => patchOAuth({ userinfo_endpoint: v })}
              />
            </div>
          </details>
        </div>
      )}

      {showStatic && (
        <div className="space-y-3 rounded-md border p-3">
          <p className="text-sm font-medium">{t('mcpServers.wizard.shape.staticSection')}</p>
          <AuthHeaderFieldset
            value={{
              headerName: state.auth_header_name,
              valueTemplate: state.auth_value_template,
            }}
            onChange={setHeader}
          />
          <div className="space-y-1.5">
            <Label htmlFor="static-help-url" className="text-xs">
              {t('mcpServers.wizard.staticHelpUrl')}
            </Label>
            <Input
              id="static-help-url"
              value={state.static_token_help_url}
              onChange={(e) => patch({ static_token_help_url: e.target.value })}
              placeholder="https://github.com/settings/tokens"
            />
            <p className="text-xs text-muted-foreground">
              {t('mcpServers.wizard.staticHelpUrlHint')}
            </p>
          </div>
        </div>
      )}

      <div className="flex items-center justify-between gap-2 pt-2">
        <Button variant="outline" type="button" onClick={onBack}>
          <ArrowLeft className="h-4 w-4" />
          {t('common.back')}
        </Button>
        <Button type="button" onClick={onNext} disabled={!canProceed}>
          {t('common.next')}
        </Button>
      </div>
    </div>
  );
}

/**
 * Summary banner — surfaces the Step 1 probe verdict and the resulting
 * recommendation so the auto-pick isn't a black box. Three branches:
 *
 *   - `auth_required` + OAuth metadata + DCR succeeded ⇒ green; we
 *     pre-filled everything (issuer, client_id, client_secret).
 *   - `auth_required` + OAuth metadata + no DCR ⇒ amber; admin needs
 *     to register the upstream OAuth app and paste Client ID back.
 *   - `auth_required` + no OAuth metadata ⇒ neutral; static token
 *     is the recommendation.
 *
 * Anonymous-OK probes never reach Step 2 (the wizard shell skips
 * straight to Step 4), so we don't render that case here.
 */
function ProbeSummary({
  url,
  probe,
  oauthProbe,
}: {
  url: string;
  probe: ProbeResult;
  oauthProbe: OAuthProbeResult | null;
}) {
  const { t } = useTranslation();

  type Tone = 'success' | 'warn' | 'neutral';
  let tone: Tone;
  let title: string;
  let detail: string;
  let recommendation: string;

  if (oauthProbe?.issuer && oauthProbe.client_id) {
    tone = 'success';
    title = t('mcpServers.wizard.probeSummary.oauthDcrTitle');
    detail = t('mcpServers.wizard.probeSummary.oauthDcrDetail', {
      issuer: oauthProbe.issuer,
    });
    recommendation = t('mcpServers.wizard.probeSummary.oauthDcrRec');
  } else if (oauthProbe?.issuer) {
    tone = 'warn';
    title = t('mcpServers.wizard.probeSummary.oauthManualTitle');
    detail = t('mcpServers.wizard.probeSummary.oauthManualDetail', {
      issuer: oauthProbe.issuer,
    });
    recommendation = t('mcpServers.wizard.probeSummary.oauthManualRec');
  } else {
    tone = 'neutral';
    title = t('mcpServers.wizard.probeSummary.staticTitle');
    detail = probe.message;
    recommendation = t('mcpServers.wizard.probeSummary.staticRec');
  }

  return (
    <div
      className={cn(
        'rounded-md border p-3 text-xs',
        tone === 'success' &&
          'border-emerald-500/30 bg-emerald-500/10 text-emerald-900 dark:text-emerald-200',
        tone === 'warn' &&
          'border-amber-500/30 bg-amber-500/10 text-amber-900 dark:text-amber-200',
        tone === 'neutral' && 'border-border bg-muted/40 text-muted-foreground',
      )}
    >
      <div className="flex items-start gap-2">
        <Info
          className={cn(
            'mt-0.5 h-4 w-4 shrink-0',
            tone === 'success' && 'text-emerald-600 dark:text-emerald-300',
            tone === 'warn' && 'text-amber-600 dark:text-amber-300',
            tone === 'neutral' && 'text-muted-foreground',
          )}
        />
        <div className="flex-1 space-y-1">
          <p className="font-medium">{title}</p>
          <p className="opacity-85">
            {t('mcpServers.wizard.probeSummary.probedUrl')}{' '}
            <code className="font-mono">{url}</code>
          </p>
          <p className="opacity-85">{detail}</p>
          <p className="font-medium">
            {t('mcpServers.wizard.probeSummary.recommendationLabel')}: {recommendation}
          </p>
        </div>
      </div>
    </div>
  );
}

function ShapeRow({
  id,
  value,
  label,
  hint,
  selected,
}: {
  id: string;
  value: AuthShape;
  label: string;
  hint: string;
  selected: boolean;
}) {
  return (
    <Label
      htmlFor={`shape-${id}`}
      className={cn(
        'flex cursor-pointer items-start gap-3 rounded-md border p-3 transition-colors',
        selected ? 'border-primary bg-primary/5' : 'hover:bg-muted/50',
      )}
    >
      <RadioGroupItem id={`shape-${id}`} value={value} className="mt-0.5" />
      <div className="flex-1 space-y-0.5">
        <div className="flex items-center gap-2 text-sm font-medium">
          {label}
          {selected && <Check className="h-3.5 w-3.5 text-primary" />}
        </div>
        <p className="text-xs font-normal text-muted-foreground">{hint}</p>
      </div>
    </Label>
  );
}

function Field({
  id,
  label,
  value,
  onChange,
  type,
  required,
  placeholder,
  hint,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (v: string) => void;
  type?: 'text' | 'password';
  required?: boolean;
  placeholder?: string;
  hint?: string;
}) {
  return (
    <div className="space-y-1.5">
      <Label htmlFor={id} className="text-xs">
        {label}
        {required && <span className="text-destructive"> *</span>}
      </Label>
      <Input
        id={id}
        type={type ?? 'text'}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        autoComplete="off"
      />
      {hint && <p className="text-[11px] text-muted-foreground">{hint}</p>}
    </div>
  );
}
