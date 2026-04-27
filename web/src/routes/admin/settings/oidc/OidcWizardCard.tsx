import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Separator } from '@/components/ui/separator';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  AlertCircle,
  Check,
  Copy,
  ExternalLink,
  Loader2,
  RefreshCw,
} from 'lucide-react';
import { toast } from 'sonner';
import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api';
import type {
  OidcDiscoveryMetadata,
  OidcSettings,
  OidcTestResult,
} from '../types';
import { PROVIDER_CATALOG, type ProviderId, findPreset } from './providers';

const TEST_BROADCAST_CHANNEL = 'thinkwatch-sso-test';

/// The OIDC SSO setup card, redesigned as a step-by-step wizard.
///
/// Mental model:
///  * **Active config** is what live SSO logins use. Once configured,
///    flipping the on/off switch is the entire first-page UI.
///  * **Draft config** is what the wizard mutates. It's persisted on
///    the server so the admin can come back tomorrow with the
///    client_secret from their security team and pick up where they
///    left off.
///  * **Activation** copies the draft into the active slot, but only
///    after a recent successful test login proves the credentials
///    actually work — so it's impossible to silently break SSO via
///    config edits.
export function OidcWizardCard() {
  const { t } = useTranslation();
  const canEdit = hasPermission('settings:write');
  const [data, setData] = useState<OidcSettings | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Whether the admin is currently editing (draft visible). When
  // there's no draft and the active config is set up, the card
  // collapses to the on/off summary.
  const [editing, setEditing] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const d = await api<OidcSettings>('/api/admin/settings/oidc');
      setData(d);
      // Auto-enter editing mode when there's already a draft (the
      // admin's previous session). Otherwise stay collapsed and
      // wait for "Edit config" / "Set up SSO".
      setEditing((prev) => prev || !!d.draft);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load OIDC settings');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('settingsPage.oidcTitle')}</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-xs italic text-muted-foreground">{t('common.loading')}</p>
        </CardContent>
      </Card>
    );
  }

  if (error || !data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('settingsPage.oidcTitle')}</CardTitle>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>{error ?? 'Unknown error'}</AlertDescription>
          </Alert>
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base flex items-center gap-2">
          {t('settingsPage.oidcTitle')}
          {data.active.enabled && data.active.configured && (
            <Badge variant="outline" className="text-emerald-500 border-emerald-500/50">
              {t('settings.oidc.statusActive')}
            </Badge>
          )}
          {data.draft && (
            <Badge variant="outline" className="text-amber-500 border-amber-500/50">
              {t('settings.oidc.statusDraft')}
            </Badge>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <ActiveSummary
          data={data}
          editing={editing}
          canEdit={canEdit}
          onEdit={() => setEditing(true)}
          onReload={reload}
        />
        {(editing || !data.active.configured) && (
          <>
            {data.active.configured && <Separator />}
            <Wizard data={data} canEdit={canEdit} onReload={reload} onClose={() => setEditing(false)} />
          </>
        )}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Active-config summary
// ---------------------------------------------------------------------------

interface ActiveSummaryProps {
  data: OidcSettings;
  editing: boolean;
  canEdit: boolean;
  onEdit: () => void;
  onReload: () => Promise<void>;
}

function ActiveSummary({ data, editing, canEdit, onEdit, onReload }: ActiveSummaryProps) {
  const { t } = useTranslation();
  const { active } = data;
  if (!active.configured) {
    // Fresh install — wizard is the whole UI.
    return null;
  }

  const toggleEnabled = async (next: boolean) => {
    try {
      await apiPatch('/api/admin/settings/oidc', { enabled: next });
      toast.success(next ? t('settings.oidc.enabledToast') : t('settings.oidc.disabledToast'));
      await onReload();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Toggle failed');
    }
  };

  const provider = findPreset(active.provider_preset);
  const providerLabel = provider ? t(provider.labelKey) : t('settings.oidc.providers.generic');

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <div>
          <Label className="text-sm">{t('settings.oidc.enableSsoLabel')}</Label>
          <p className="text-xs text-muted-foreground mt-0.5">
            {t('settings.oidc.enableSsoHint')}
          </p>
        </div>
        <Switch
          checked={active.enabled}
          onCheckedChange={toggleEnabled}
          disabled={!canEdit}
        />
      </div>
      <dl className="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1 text-sm">
        <dt className="text-muted-foreground">{t('settings.oidc.summaryProvider')}</dt>
        <dd>{providerLabel}</dd>
        <dt className="text-muted-foreground">{t('settings.oidc.summaryIssuer')}</dt>
        <dd className="font-mono text-xs break-all">{active.issuer_url ?? '—'}</dd>
        <dt className="text-muted-foreground">{t('settings.oidc.summaryClientId')}</dt>
        <dd className="font-mono text-xs break-all">{active.client_id ?? '—'}</dd>
        <dt className="text-muted-foreground">{t('settings.oidc.summaryEmailClaim')}</dt>
        <dd className="font-mono text-xs">{active.email_claim}</dd>
      </dl>
      {!editing && (
        <div className="pt-1">
          <Button size="sm" variant="outline" onClick={onEdit} disabled={!canEdit}>
            {data.draft ? t('settings.oidc.resumeDraft') : t('settings.oidc.editConfig')}
          </Button>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Wizard body — the 5 steps
// ---------------------------------------------------------------------------

interface WizardProps {
  data: OidcSettings;
  canEdit: boolean;
  onReload: () => Promise<void>;
  onClose: () => void;
}

function Wizard({ data, canEdit, onReload, onClose }: WizardProps) {
  const { t } = useTranslation();
  const draft = data.draft;
  const test = data.test_result;

  // Per-step "completion" predicates. Used to gate the next step
  // and to render the ✓ tick.
  const stepIssuerDone = !!draft?.issuer_url;
  const stepRedirectDone = !!draft?.redirect_url;
  const stepCredsDone = !!draft?.client_id && (draft?.has_secret ?? false);
  const stepTestDone = test?.passed === true;

  return (
    <div className="space-y-6">
      {data.active.configured && (
        <Alert>
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>
            {t('settings.oidc.editingDraftBanner')}
          </AlertDescription>
        </Alert>
      )}

      <Step
        n={1}
        done={stepIssuerDone}
        active={!stepIssuerDone}
        title={t('settings.oidc.step1Title')}
        hint={t('settings.oidc.step1Hint')}
      >
        <ProviderAndIssuerStep draft={draft} canEdit={canEdit} onSaved={onReload} />
      </Step>

      <Step
        n={2}
        done={stepRedirectDone}
        active={stepIssuerDone && !stepRedirectDone}
        disabled={!stepIssuerDone}
        title={t('settings.oidc.step2Title')}
        hint={t('settings.oidc.step2Hint')}
      >
        <RedirectUrlStep
          draft={draft}
          defaultRedirect={data.default_redirect_url}
          canEdit={canEdit}
          onSaved={onReload}
        />
      </Step>

      <Step
        n={3}
        done={stepCredsDone}
        active={stepRedirectDone && !stepCredsDone}
        disabled={!stepRedirectDone}
        title={t('settings.oidc.step3Title')}
        hint={t('settings.oidc.step3Hint')}
      >
        <CredentialsStep draft={draft} canEdit={canEdit} onSaved={onReload} />
      </Step>

      <Step
        n={4}
        done={stepTestDone}
        active={stepCredsDone && !stepTestDone}
        disabled={!stepCredsDone}
        title={t('settings.oidc.step4Title')}
        hint={t('settings.oidc.step4Hint')}
      >
        <TestLoginStep test={test} canEdit={canEdit} onResult={onReload} />
      </Step>

      <Step
        n={5}
        done={false}
        active={stepTestDone}
        disabled={!stepTestDone}
        title={t('settings.oidc.step5Title')}
        hint={t('settings.oidc.step5Hint')}
      >
        <ActivateStep canEdit={canEdit} onActivated={onReload} />
      </Step>

      <div className="flex justify-end gap-2 pt-2">
        <Button
          variant="ghost"
          size="sm"
          onClick={async () => {
            try {
              await apiDelete('/api/admin/settings/oidc/draft');
              toast.success(t('settings.oidc.draftDiscarded'));
              await onReload();
              onClose();
            } catch (e) {
              toast.error(e instanceof Error ? e.message : 'Discard failed');
            }
          }}
          disabled={!canEdit || !draft}
        >
          {t('settings.oidc.discardDraft')}
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step shell
// ---------------------------------------------------------------------------

interface StepProps {
  n: number;
  done: boolean;
  active: boolean;
  disabled?: boolean;
  title: string;
  hint?: string;
  children: React.ReactNode;
}

function Step({ n, done, active, disabled, title, hint, children }: StepProps) {
  return (
    <div className={`flex gap-4 ${disabled ? 'opacity-50' : ''}`}>
      <div className="flex flex-col items-center pt-1">
        <div
          className={`flex h-7 w-7 items-center justify-center rounded-full border text-xs font-medium ${
            done
              ? 'bg-emerald-500/15 border-emerald-500 text-emerald-500'
              : active
                ? 'bg-primary/15 border-primary text-primary'
                : 'bg-muted border-muted-foreground/30 text-muted-foreground'
          }`}
        >
          {done ? <Check className="h-4 w-4" /> : n}
        </div>
      </div>
      <div className="flex-1 space-y-2">
        <div>
          <h3 className="text-sm font-medium leading-tight">{title}</h3>
          {hint && <p className="text-xs text-muted-foreground mt-0.5">{hint}</p>}
        </div>
        <div className={disabled ? 'pointer-events-none' : undefined}>{children}</div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 1 — provider + issuer
// ---------------------------------------------------------------------------

interface ProviderAndIssuerStepProps {
  draft: OidcSettings['draft'];
  canEdit: boolean;
  onSaved: () => Promise<void>;
}

function ProviderAndIssuerStep({ draft, canEdit, onSaved }: ProviderAndIssuerStepProps) {
  const { t } = useTranslation();
  const [provider, setProvider] = useState<ProviderId>(
    (draft?.provider_preset as ProviderId) ?? 'generic',
  );
  const [issuer, setIssuer] = useState(draft?.issuer_url ?? '');
  const [verifying, setVerifying] = useState(false);
  const [verifyError, setVerifyError] = useState<string | null>(null);
  const [discovery, setDiscovery] = useState<OidcDiscoveryMetadata | null>(null);

  // Re-sync local state when the draft changes externally (the parent
  // refetches after every step).
  useEffect(() => {
    setProvider((draft?.provider_preset as ProviderId) ?? 'generic');
    setIssuer(draft?.issuer_url ?? '');
  }, [draft?.provider_preset, draft?.issuer_url]);

  const preset = findPreset(provider);

  const onProviderChange = (id: ProviderId) => {
    setProvider(id);
    const p = findPreset(id);
    if (p?.defaultIssuer && !issuer) setIssuer(p.defaultIssuer);
  };

  const onVerify = async () => {
    if (!issuer) return;
    setVerifying(true);
    setVerifyError(null);
    setDiscovery(null);
    try {
      // Save the draft first so the server-side discover endpoint
      // has something to work with. Sending the claim mapping
      // defaults at the same time means step 3's claim fields show
      // sensible values when the admin gets there.
      await apiPatch('/api/admin/settings/oidc/draft', {
        provider_preset: provider,
        issuer_url: issuer.trim(),
        email_claim: preset?.defaultEmailClaim ?? 'email',
        name_claim: preset?.defaultNameClaim ?? 'name',
      });
      const meta = await apiPost<OidcDiscoveryMetadata>(
        '/api/admin/settings/oidc/discover',
        {},
      );
      setDiscovery(meta);
      await onSaved();
    } catch (e) {
      setVerifyError(e instanceof Error ? e.message : 'Discovery failed');
    } finally {
      setVerifying(false);
    }
  };

  return (
    <div className="space-y-3">
      <div className="grid gap-3 sm:grid-cols-[200px_1fr]">
        <div>
          <Label className="text-xs">{t('settings.oidc.providerLabel')}</Label>
          <Select value={provider} onValueChange={(v) => onProviderChange(v as ProviderId)} disabled={!canEdit}>
            <SelectTrigger className="mt-1 h-9">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PROVIDER_CATALOG.map((p) => (
                <SelectItem key={p.id} value={p.id}>
                  {t(p.labelKey)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div>
          <Label className="text-xs">{t('settings.oidc.issuerLabel')}</Label>
          <Input
            className="mt-1 font-mono text-xs"
            value={issuer}
            onChange={(e) => setIssuer(e.target.value)}
            placeholder={preset?.issuerPlaceholder ?? 'https://idp.example.com/'}
            disabled={!canEdit}
          />
        </div>
      </div>
      {verifyError && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{verifyError}</AlertDescription>
        </Alert>
      )}
      {discovery && (
        <Alert>
          <Check className="h-4 w-4" />
          <AlertDescription>
            <div className="text-xs space-y-0.5">
              <div>
                <span className="text-muted-foreground">authorize:</span>{' '}
                <span className="font-mono break-all">{discovery.authorization_endpoint}</span>
              </div>
              <div>
                <span className="text-muted-foreground">token:</span>{' '}
                <span className="font-mono break-all">{discovery.token_endpoint}</span>
              </div>
              {discovery.jwks_uri && (
                <div>
                  <span className="text-muted-foreground">jwks:</span>{' '}
                  <span className="font-mono break-all">{discovery.jwks_uri}</span>
                </div>
              )}
            </div>
          </AlertDescription>
        </Alert>
      )}
      <div className="flex items-center gap-2">
        <Button size="sm" onClick={onVerify} disabled={!canEdit || !issuer || verifying}>
          {verifying ? <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" /> : null}
          {t('settings.oidc.verifyIssuer')}
        </Button>
        {preset?.docsUrl && (
          <a
            href={preset.docsUrl}
            target="_blank"
            rel="noopener noreferrer"
            className="text-xs text-muted-foreground hover:text-foreground inline-flex items-center gap-1"
          >
            {t('settings.oidc.openProviderDocs')}
            <ExternalLink className="h-3 w-3" />
          </a>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 2 — redirect URL (read-only-ish, copy)
// ---------------------------------------------------------------------------

interface RedirectUrlStepProps {
  draft: OidcSettings['draft'];
  defaultRedirect: string;
  canEdit: boolean;
  onSaved: () => Promise<void>;
}

function RedirectUrlStep({ draft, defaultRedirect, canEdit, onSaved }: RedirectUrlStepProps) {
  const { t } = useTranslation();
  const value = draft?.redirect_url ?? defaultRedirect;
  const confirmed = !!draft?.redirect_url;

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      toast.success(t('settings.oidc.copied'));
    } catch {
      toast.error('Clipboard unavailable');
    }
  };

  const confirm = async () => {
    try {
      await apiPatch('/api/admin/settings/oidc/draft', { redirect_url: value });
      await onSaved();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Save failed');
    }
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <Input className="font-mono text-xs" value={value} readOnly />
        <Button size="sm" variant="outline" onClick={copy} type="button">
          <Copy className="h-3.5 w-3.5" />
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">{t('settings.oidc.redirectInstruction')}</p>
      {!confirmed && (
        <Button size="sm" onClick={confirm} disabled={!canEdit}>
          {t('settings.oidc.redirectConfirm')}
        </Button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 3 — credentials + claim mapping
// ---------------------------------------------------------------------------

interface CredentialsStepProps {
  draft: OidcSettings['draft'];
  canEdit: boolean;
  onSaved: () => Promise<void>;
}

function CredentialsStep({ draft, canEdit, onSaved }: CredentialsStepProps) {
  const { t } = useTranslation();
  const [clientId, setClientId] = useState(draft?.client_id ?? '');
  const [clientSecret, setClientSecret] = useState('');
  const [emailClaim, setEmailClaim] = useState(draft?.email_claim ?? 'email');
  const [nameClaim, setNameClaim] = useState(draft?.name_claim ?? 'name');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setClientId(draft?.client_id ?? '');
    setEmailClaim(draft?.email_claim ?? 'email');
    setNameClaim(draft?.name_claim ?? 'name');
  }, [draft?.client_id, draft?.email_claim, draft?.name_claim]);

  const provider = findPreset(draft?.provider_preset);
  const hasSecret = draft?.has_secret ?? false;

  const save = async () => {
    setSaving(true);
    try {
      await apiPatch('/api/admin/settings/oidc/draft', {
        client_id: clientId.trim(),
        ...(clientSecret ? { client_secret: clientSecret } : {}),
        email_claim: emailClaim || 'email',
        name_claim: nameClaim || 'name',
      });
      setClientSecret('');
      await onSaved();
      toast.success(t('settings.oidc.credentialsSaved'));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-3">
      {provider && provider.credentialsHintKey && (
        <p className="text-xs text-muted-foreground">{t(provider.credentialsHintKey)}</p>
      )}
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <Label className="text-xs">{t('settings.oidc.clientIdLabel')}</Label>
          <Input
            className="mt-1 font-mono text-xs"
            value={clientId}
            onChange={(e) => setClientId(e.target.value)}
            placeholder="abc123…"
            disabled={!canEdit}
          />
        </div>
        <div>
          <Label className="text-xs">{t('settings.oidc.clientSecretLabel')}</Label>
          <Input
            type="password"
            className="mt-1"
            value={clientSecret}
            onChange={(e) => setClientSecret(e.target.value)}
            placeholder={hasSecret ? t('settings.oidc.clientSecretKeep') : t('settings.oidc.clientSecretEnter')}
            disabled={!canEdit}
          />
          {hasSecret && (
            <p className="text-xs text-muted-foreground mt-1">{t('settings.oidc.clientSecretConfigured')}</p>
          )}
        </div>
      </div>
      <details className="text-xs">
        <summary className="cursor-pointer text-muted-foreground hover:text-foreground">
          {t('settings.oidc.advancedClaims')}
        </summary>
        <div className="grid gap-3 sm:grid-cols-2 mt-2">
          <div>
            <Label className="text-xs">{t('settings.oidc.emailClaimLabel')}</Label>
            <Input
              className="mt-1 font-mono text-xs"
              value={emailClaim}
              onChange={(e) => setEmailClaim(e.target.value)}
              placeholder="email"
              disabled={!canEdit}
            />
          </div>
          <div>
            <Label className="text-xs">{t('settings.oidc.nameClaimLabel')}</Label>
            <Input
              className="mt-1 font-mono text-xs"
              value={nameClaim}
              onChange={(e) => setNameClaim(e.target.value)}
              placeholder="name"
              disabled={!canEdit}
            />
          </div>
        </div>
      </details>
      <Button size="sm" onClick={save} disabled={!canEdit || saving || !clientId.trim()}>
        {saving ? <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" /> : null}
        {t('settings.oidc.saveCredentials')}
      </Button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 4 — test login (popup)
// ---------------------------------------------------------------------------

interface TestLoginStepProps {
  test: OidcTestResult | null;
  canEdit: boolean;
  onResult: () => Promise<void>;
}

function TestLoginStep({ test, canEdit, onResult }: TestLoginStepProps) {
  const { t } = useTranslation();
  const [launching, setLaunching] = useState(false);
  const [popupOpen, setPopupOpen] = useState(false);
  const popupRef = useRef<Window | null>(null);

  // Listen for the popup's BroadcastChannel completion message and
  // refresh the wizard's view when it arrives. Also poll the popup's
  // closed state so we can refresh even when BroadcastChannel is
  // blocked (e.g. cross-origin iframes, very old browsers).
  useEffect(() => {
    if (!popupOpen) return;
    let cancelled = false;
    let channel: BroadcastChannel | null = null;
    try {
      channel = new BroadcastChannel(TEST_BROADCAST_CHANNEL);
      channel.onmessage = () => {
        if (!cancelled) {
          void onResult();
          setPopupOpen(false);
        }
      };
    } catch {
      // BroadcastChannel unsupported — fall through to closed-poll.
    }
    const poll = window.setInterval(() => {
      if (cancelled) return;
      if (popupRef.current?.closed) {
        window.clearInterval(poll);
        void onResult();
        setPopupOpen(false);
      }
    }, 500);
    return () => {
      cancelled = true;
      if (channel) channel.close();
      window.clearInterval(poll);
    };
  }, [popupOpen, onResult]);

  const launch = async () => {
    setLaunching(true);
    try {
      const { authorize_url } = await apiPost<{ authorize_url: string; state: string }>(
        '/api/admin/settings/oidc/test-login',
        {},
      );
      const popup = window.open(
        authorize_url,
        'thinkwatch-sso-test',
        'width=540,height=720,resizable=yes,scrollbars=yes',
      );
      if (!popup) {
        toast.error(t('settings.oidc.popupBlocked'));
        return;
      }
      popupRef.current = popup;
      setPopupOpen(true);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Test launch failed');
    } finally {
      setLaunching(false);
    }
  };

  return (
    <div className="space-y-3">
      {test && (
        <Alert variant={test.passed ? 'default' : 'destructive'}>
          {test.passed ? <Check className="h-4 w-4" /> : <AlertCircle className="h-4 w-4" />}
          <AlertDescription>
            <div className="text-xs">
              {test.passed ? (
                <>
                  <div>
                    {t('settings.oidc.testPassed', {
                      ago: relativeTime(test.at),
                    })}
                  </div>
                  {test.claims_preview && (
                    <div className="mt-1 font-mono break-all">
                      {test.claims_preview.email ?? test.claims_preview.subject}
                    </div>
                  )}
                </>
              ) : (
                <>
                  <div>{t('settings.oidc.testFailed')}</div>
                  {test.error && <div className="mt-1 font-mono break-all">{test.error}</div>}
                </>
              )}
            </div>
          </AlertDescription>
        </Alert>
      )}
      <div className="flex items-center gap-2">
        <Button size="sm" onClick={launch} disabled={!canEdit || launching || popupOpen}>
          {launching || popupOpen ? <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="mr-2 h-3.5 w-3.5" />}
          {test?.passed ? t('settings.oidc.testAgain') : t('settings.oidc.testLogin')}
        </Button>
        {popupOpen && (
          <span className="text-xs text-muted-foreground">{t('settings.oidc.testWaiting')}</span>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step 5 — activate
// ---------------------------------------------------------------------------

interface ActivateStepProps {
  canEdit: boolean;
  onActivated: () => Promise<void>;
}

function ActivateStep({ canEdit, onActivated }: ActivateStepProps) {
  const { t } = useTranslation();
  const [activating, setActivating] = useState(false);

  const activate = async () => {
    setActivating(true);
    try {
      await apiPost('/api/admin/settings/oidc/activate', {});
      toast.success(t('settings.oidc.activated'));
      await onActivated();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Activation failed');
    } finally {
      setActivating(false);
    }
  };

  return (
    <Button size="sm" onClick={activate} disabled={!canEdit || activating}>
      {activating ? <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" /> : null}
      {t('settings.oidc.activateConfig')}
    </Button>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function relativeTime(epochSecs: number): string {
  const now = Date.now() / 1000;
  const diff = Math.max(0, now - epochSecs);
  if (diff < 60) return `${Math.round(diff)}s ago`;
  if (diff < 3600) return `${Math.round(diff / 60)}m ago`;
  return `${Math.round(diff / 3600)}h ago`;
}
