import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, ArrowLeft, CheckCircle2, ChevronDown, Loader2, Sparkles } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from '@/components/ui/collapsible';
import { DialogFooter } from '@/components/ui/dialog';
import { HeaderEditor } from '@/components/header-editor';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiPost } from '@/lib/api';
import { resolveCollision, sanitizePrefixInput, slugifyPrefix } from '@/lib/prefix-utils';
import { cn } from '@/lib/utils';
import { AuthModePicker, type AuthMode } from './auth-mode-picker';
import {
  emptyOAuth,
  oauthPayload,
  OAuthFieldset,
  type OAuthFields,
} from './oauth-fieldset';
import { McpTestPanel, type McpTestResult } from './test-panel';
import { toast } from 'sonner';

interface ServerWizardProps {
  taken: { names: Set<string>; prefixes: Set<string> };
  onSuccess: () => void;
  onCancel: () => void;
}

type Step = 1 | 2 | 3;

export function ServerWizard({ taken, onSuccess, onCancel }: ServerWizardProps) {
  const { t } = useTranslation();
  const [step, setStep] = useState<Step>(1);
  const [mode, setMode] = useState<AuthMode | null>(null);

  // Step 2 form state
  const [name, setName] = useState('');
  const [namespacePrefix, setNamespacePrefix] = useState('');
  const [prefixManuallyEdited, setPrefixManuallyEdited] = useState(false);
  const [description, setDescription] = useState('');
  const [endpointUrl, setEndpointUrl] = useState('');
  const [oauth, setOauth] = useState<OAuthFields>(emptyOAuth());
  const [allowStaticTokenFallback, setAllowStaticTokenFallback] = useState(false);
  const [staticTokenHelpUrl, setStaticTokenHelpUrl] = useState('');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>([]);
  const [cacheTtl, setCacheTtl] = useState('');
  const [step2Error, setStep2Error] = useState('');

  // Step 3 state
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<McpTestResult | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState('');

  // OAuth auto-discovery (Step 2, OAuth mode only). The probe runs the
  // RFC 9728 → 8414 → 7591 chain server-side and returns whatever it
  // could derive — we silently fill the form on full success, leave
  // the OAuthFieldset visible underneath for review/override on
  // partial success or failure.
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<{
    kind: 'success' | 'partial' | 'failure';
    issuer?: string;
    isPublicClient: boolean;
    redirectUri: string;
    diagnostic: string[];
  } | null>(null);

  const resolved = useMemo(() => {
    if (!name.trim()) return null;
    const basePrefix = prefixManuallyEdited && namespacePrefix
      ? namespacePrefix
      : slugifyPrefix(name);
    if (!basePrefix) return null;
    return resolveCollision(name.trim(), basePrefix, taken.names, taken.prefixes);
  }, [name, namespacePrefix, prefixManuallyEdited, taken]);

  const handleSelectMode = (m: AuthMode) => {
    setMode(m);
    setStep(2);
  };

  const allowStaticToken = mode === 'static' || (mode === 'oauth' && allowStaticTokenFallback);

  const buildHeaders = () =>
    customHeaders.length > 0
      ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
      : null;

  const validateStep2 = (): string | null => {
    if (!name.trim()) return t('mcpServers.wizard.errors.nameRequired');
    if (!endpointUrl.trim()) return t('mcpServers.wizard.errors.endpointRequired');
    if (mode === 'oauth' && !oauth.issuer.trim()) {
      return t('mcpServers.wizard.errors.issuerRequired');
    }
    return null;
  };

  const goToStep3 = () => {
    const err = validateStep2();
    if (err) {
      setStep2Error(err);
      return;
    }
    setStep2Error('');
    setStep(3);
  };

  const runProbe = async () => {
    if (!endpointUrl.trim()) {
      toast.error(t('mcpServers.wizard.errors.endpointRequired'));
      return;
    }
    setProbing(true);
    setProbeResult(null);
    try {
      const meta = await apiPost<{
        issuer?: string;
        authorization_endpoint?: string;
        token_endpoint?: string;
        revocation_endpoint?: string;
        userinfo_endpoint?: string;
        registration_endpoint?: string;
        scopes_supported?: string[];
        client_id?: string;
        client_secret?: string;
        is_public_client: boolean;
        redirect_uri: string;
        diagnostic: string[];
      }>('/api/admin/mcp/oauth-probe', { endpoint_url: endpointUrl.trim() });

      // Merge into the OAuthFieldset state. We *replace* every field
      // on success — the admin pasted a URL and asked us to figure
      // out everything, so leaving stale half-filled values would be
      // worse than wiping. If they prefer a hybrid, the fieldset is
      // still right below for hand-tweaking.
      setOauth({
        issuer: meta.issuer ?? '',
        authorizationEndpoint: meta.authorization_endpoint ?? '',
        tokenEndpoint: meta.token_endpoint ?? '',
        revocationEndpoint: meta.revocation_endpoint ?? '',
        userinfoEndpoint: meta.userinfo_endpoint ?? '',
        clientId: meta.client_id ?? '',
        clientSecret: meta.client_secret ?? '',
        scopes: (meta.scopes_supported ?? []).join(' '),
      });

      const haveCore = !!(meta.authorization_endpoint && meta.token_endpoint);
      const haveClient = !!meta.client_id;
      const kind: 'success' | 'partial' | 'failure' = haveCore && haveClient
        ? 'success'
        : haveCore
          ? 'partial'
          : 'failure';
      setProbeResult({
        kind,
        issuer: meta.issuer,
        isPublicClient: meta.is_public_client,
        redirectUri: meta.redirect_uri,
        diagnostic: meta.diagnostic,
      });
    } catch (err) {
      setProbeResult({
        kind: 'failure',
        isPublicClient: false,
        redirectUri: '',
        diagnostic: [err instanceof Error ? err.message : 'probe failed'],
      });
    } finally {
      setProbing(false);
    }
  };

  const runTest = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      const res = await apiPost<McpTestResult>('/api/mcp/servers/test', {
        endpoint_url: endpointUrl,
        custom_headers: buildHeaders(),
      });
      setTestResult(res);
    } catch (err) {
      setTestResult({
        success: false,
        message: err instanceof Error ? err.message : 'Connection failed',
      });
    } finally {
      setTesting(false);
    }
  };

  // Auto-test on entering Step 3
  useEffect(() => {
    if (step === 3 && !testResult && !testing) {
      void runTest();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step]);

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!testResult?.success) return;
    setSubmitting(true);
    setSubmitError('');
    try {
      await apiPost('/api/mcp/servers', {
        name: resolved?.name ?? name,
        namespace_prefix: resolved?.prefix ?? (namespacePrefix || undefined),
        description,
        endpoint_url: endpointUrl,
        ...(mode === 'oauth' ? oauthPayload(oauth, true) : {}),
        allow_static_token: allowStaticToken,
        static_token_help_url: allowStaticToken ? (staticTokenHelpUrl || null) : null,
        custom_headers: buildHeaders(),
        cache_ttl_secs: cacheTtl ? Number(cacheTtl) : undefined,
      });
      // Surface "next step" guidance — for OAuth/PAT servers the
      // admin's job isn't done; users still need to authorize at
      // /connections. For public/headers servers the gateway can
      // already invoke tools, so just confirm.
      if (mode === 'oauth' || allowStaticToken) {
        toast.success(t('mcpServers.wizard.savedNextConnections'), {
          duration: 8000,
          action: {
            label: t('mcpStore.goToConnections'),
            onClick: () => {
              window.location.href = '/connections';
            },
          },
        });
      } else {
        toast.success(t('mcpServers.wizard.savedReady'));
      }
      onSuccess();
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : 'Failed to register server');
    } finally {
      setSubmitting(false);
    }
  };

  // Jumping back from Step 3 wipes the test result so the auto-test
  // re-runs cleanly when the user advances again. Jumping forward isn't
  // allowed — the indicator only renders past steps as buttons.
  const handleJump = (n: Step) => {
    if (n >= step) return;
    if (step === 3) setTestResult(null);
    setStep(n);
  };

  return (
    <div className="space-y-4">
      <StepIndicator step={step} onJump={handleJump} />

      {step === 1 && (
        <>
          <p className="text-sm text-muted-foreground">
            {t('mcpServers.wizard.step1Hint')}
          </p>
          <AuthModePicker value={mode} onChange={handleSelectMode} />
          <p className="text-xs text-muted-foreground">
            {t('mcpServers.wizard.lookingForKnown')}{' '}
            <a href="/mcp/store" className="underline hover:text-foreground">
              {t('mcpServers.wizard.browseStore')}
            </a>
          </p>
          <DialogFooter>
            <Button variant="outline" type="button" onClick={onCancel}>
              {t('common.cancel')}
            </Button>
          </DialogFooter>
        </>
      )}

      {step === 2 && mode && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            goToStep3();
          }}
          className="space-y-4"
        >
          {step2Error && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{step2Error}</AlertDescription>
            </Alert>
          )}

          <div className="space-y-2">
            <Label htmlFor="wiz-name">{t('common.name')}</Label>
            <Input
              id="wiz-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="my-mcp-server"
              required
              autoFocus
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="wiz-prefix">{t('mcpServers.namespacePrefix')}</Label>
            <Input
              id="wiz-prefix"
              value={prefixManuallyEdited ? namespacePrefix : (resolved?.prefix ?? slugifyPrefix(name))}
              onChange={(e) => {
                setPrefixManuallyEdited(true);
                setNamespacePrefix(sanitizePrefixInput(e.target.value));
              }}
              placeholder={t('mcpServers.namespacePrefixPlaceholder')}
              pattern="[a-z0-9_]{1,32}"
              maxLength={32}
            />
            {resolved && (
              <p className="text-xs text-muted-foreground">
                {t('mcpServers.willBeStoredAs')}{' '}
                <code className="rounded bg-muted px-1 font-mono">{resolved.name}</code>
                {' / '}
                <code className="rounded bg-muted px-1 font-mono">{resolved.prefix}</code>
              </p>
            )}
            <p className="text-xs text-muted-foreground">{t('mcpServers.namespacePrefixHint')}</p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="wiz-desc">{t('common.description')}</Label>
            <Input
              id="wiz-desc"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="Code analysis tools"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="wiz-url">{t('mcpServers.endpointUrl')}</Label>
            <Input
              id="wiz-url"
              value={endpointUrl}
              onChange={(e) => setEndpointUrl(e.target.value)}
              placeholder="http://localhost:8081/mcp"
              required
            />
          </div>

          {mode === 'oauth' && (
            <>
              <div className="rounded-md border border-dashed p-3 space-y-2">
                <div className="flex items-center justify-between gap-2">
                  <div className="space-y-0.5">
                    <p className="text-sm font-medium">{t('mcpServers.oauth.probeTitle')}</p>
                    <p className="text-xs text-muted-foreground">
                      {t('mcpServers.oauth.probeHint')}
                    </p>
                  </div>
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    disabled={probing || !endpointUrl.trim()}
                    onClick={runProbe}
                  >
                    {probing ? (
                      <Loader2 className="h-3 w-3 animate-spin" />
                    ) : (
                      <Sparkles className="h-3 w-3" />
                    )}
                    {probing
                      ? t('mcpServers.oauth.probing')
                      : t('mcpServers.oauth.probeAction')}
                  </Button>
                </div>
                {probeResult && (
                  <div
                    className={cn(
                      'flex items-start gap-2 rounded-sm p-2 text-xs',
                      probeResult.kind === 'success' &&
                        'bg-green-50 text-green-800 dark:bg-green-950/30 dark:text-green-300',
                      probeResult.kind === 'partial' &&
                        'bg-amber-50 text-amber-800 dark:bg-amber-950/30 dark:text-amber-300',
                      probeResult.kind === 'failure' &&
                        'bg-red-50 text-red-800 dark:bg-red-950/30 dark:text-red-300',
                    )}
                  >
                    {probeResult.kind === 'success' ? (
                      <CheckCircle2 className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                    ) : (
                      <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                    )}
                    <div className="flex-1 space-y-2">
                      <p className="font-medium">
                        {probeResult.kind === 'success'
                          ? t('mcpServers.oauth.probeSuccess', { issuer: probeResult.issuer ?? '' })
                          : probeResult.kind === 'partial'
                            ? t('mcpServers.oauth.probePartial')
                            : t('mcpServers.oauth.probeFailure')}
                      </p>
                      {probeResult.kind === 'partial' && probeResult.redirectUri && (
                        <div className="space-y-1">
                          <p className="font-medium">
                            {t('mcpServers.oauth.partialNextSteps')}
                          </p>
                          <ol className="list-decimal space-y-1 pl-4">
                            <li>
                              {t('mcpServers.oauth.partialStepCopyUri')}
                              <div className="mt-1 flex items-center gap-1">
                                <code className="flex-1 truncate rounded bg-background/50 px-1 py-0.5 font-mono text-[11px]">
                                  {probeResult.redirectUri}
                                </code>
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="ghost"
                                  className="h-6 px-2 text-[11px]"
                                  onClick={() => {
                                    void navigator.clipboard.writeText(probeResult.redirectUri);
                                    toast.success(t('common.copied'));
                                  }}
                                >
                                  {t('common.copy')}
                                </Button>
                              </div>
                            </li>
                            <li>
                              {probeResult.issuer
                                ? t('mcpServers.oauth.partialStepRegister', {
                                    issuer: new URL(probeResult.issuer).host,
                                  })
                                : t('mcpServers.oauth.partialStepRegisterGeneric')}
                            </li>
                            <li>
                              {probeResult.isPublicClient
                                ? t('mcpServers.oauth.partialStepPasteIdOnly')
                                : t('mcpServers.oauth.partialStepPasteIdSecret')}
                            </li>
                          </ol>
                        </div>
                      )}
                      {probeResult.kind !== 'success' && probeResult.diagnostic.length > 0 && (
                        <details className="group">
                          <summary className="cursor-pointer select-none opacity-75 hover:opacity-100">
                            {t('mcpServers.oauth.probeDetails')}
                          </summary>
                          <ol className="mt-1 list-decimal space-y-0.5 pl-4 font-mono text-[11px] opacity-75">
                            {probeResult.diagnostic.map((step, i) => (
                              <li key={i} className="break-all">{step}</li>
                            ))}
                          </ol>
                        </details>
                      )}
                    </div>
                  </div>
                )}
              </div>
              <OAuthFieldset
                values={oauth}
                onChange={setOauth}
                collapsibleAdvanced
                flat
                publicClient={probeResult?.isPublicClient ?? false}
              />
              <div className="flex items-center gap-2">
                <Checkbox
                  id="wiz-allow-static-fallback"
                  checked={allowStaticTokenFallback}
                  onCheckedChange={(v) => setAllowStaticTokenFallback(v === true)}
                />
                <Label htmlFor="wiz-allow-static-fallback" className="cursor-pointer text-sm">
                  {t('mcpServers.wizard.allowStaticFallback')}
                </Label>
              </div>
              {allowStaticTokenFallback && (
                <div className="space-y-2">
                  <Label htmlFor="wiz-static-help">{t('mcpServers.wizard.staticHelpUrl')}</Label>
                  <Input
                    id="wiz-static-help"
                    value={staticTokenHelpUrl}
                    onChange={(e) => setStaticTokenHelpUrl(e.target.value)}
                    placeholder="https://github.com/settings/tokens"
                  />
                </div>
              )}
            </>
          )}

          {mode === 'static' && (
            <div className="space-y-2">
              <Label htmlFor="wiz-static-help">{t('mcpServers.wizard.staticHelpUrl')}</Label>
              <Input
                id="wiz-static-help"
                value={staticTokenHelpUrl}
                onChange={(e) => setStaticTokenHelpUrl(e.target.value)}
                placeholder="https://github.com/settings/tokens"
              />
              <p className="text-xs text-muted-foreground">
                {t('mcpServers.wizard.staticHelpUrlHint')}
              </p>
            </div>
          )}

          {mode === 'direct' && (
            <div className="space-y-2">
              <Label>{t('providers.customHeaders')}</Label>
              <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
              <HeaderEditor
                headers={customHeaders}
                onChange={setCustomHeaders}
                keyPlaceholder="X-Custom-Header"
                presets={[
                  { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                  { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
                ]}
              />
            </div>
          )}

          <Collapsible className="space-y-2">
            <CollapsibleTrigger className="group flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
              <ChevronDown className="h-3 w-3 transition-transform group-data-[state=open]:rotate-180" />
              {mode === 'direct'
                ? t('mcpServers.wizard.advancedSectionTtlOnly')
                : t('mcpServers.wizard.advancedSection')}
            </CollapsibleTrigger>
            <CollapsibleContent className="space-y-3 pt-2">
              <div className="space-y-2">
                <Label>{t('mcpServers.cacheTtlLabel')}</Label>
                <p className="text-xs text-muted-foreground">{t('mcpServers.cacheTtlHint')}</p>
                <Input
                  type="number"
                  min={0}
                  step={60}
                  placeholder={t('mcpServers.cacheTtlPlaceholder')}
                  value={cacheTtl}
                  onChange={(e) => setCacheTtl(e.target.value)}
                />
              </div>
              {mode !== 'direct' && (
                <div className="space-y-2">
                  <Label>{t('providers.customHeaders')}</Label>
                  <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
                  <HeaderEditor
                    headers={customHeaders}
                    onChange={setCustomHeaders}
                    keyPlaceholder="X-Custom-Header"
                    presets={[
                      { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                      { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
                    ]}
                  />
                </div>
              )}
            </CollapsibleContent>
          </Collapsible>

          <DialogFooter>
            <Button variant="outline" type="button" onClick={() => setStep(1)}>
              <ArrowLeft className="h-4 w-4" />
              {t('common.back')}
            </Button>
            <Button type="submit">{t('mcpServers.wizard.testAndSave')}</Button>
          </DialogFooter>
        </form>
      )}

      {step === 3 && (
        <form onSubmit={handleSubmit} className="space-y-4">
          <McpTestPanel
            testing={testing}
            result={testResult}
            onRetry={runTest}
          />

          {submitError && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{submitError}</AlertDescription>
            </Alert>
          )}

          <DialogFooter>
            <Button
              variant="outline"
              type="button"
              onClick={() => {
                setTestResult(null);
                setStep(2);
              }}
            >
              <ArrowLeft className="h-4 w-4" />
              {t('common.back')}
            </Button>
            <Button
              type="submit"
              disabled={submitting || !testResult?.success}
              title={!testResult?.success ? t('mcpServers.mustTestFirst') : undefined}
            >
              {submitting ? t('mcpServers.registering') : t('mcpServers.registerServer')}
            </Button>
          </DialogFooter>
        </form>
      )}
    </div>
  );
}

function StepIndicator({
  step,
  onJump,
}: {
  step: Step;
  onJump?: (n: Step) => void;
}) {
  const { t } = useTranslation();
  const labels: { n: Step; key: string }[] = [
    { n: 1, key: 'mcpServers.wizard.step1Title' },
    { n: 2, key: 'mcpServers.wizard.step2Title' },
    { n: 3, key: 'mcpServers.wizard.step3Title' },
  ];
  return (
    <div className="flex items-center gap-2">
      {labels.map(({ n, key }, i) => {
        const isActive = step === n;
        const isDone = step > n;
        const canJump = isDone && !!onJump;
        // Number circle is a button only when the step is in the past
        // (already completed and we have a jump handler). The current
        // and future steps render as plain divs so keyboard tab order
        // stays clean and there's no visual "click me" affordance for
        // unreachable steps.
        const numberBox = canJump ? (
          <button
            type="button"
            onClick={() => onJump(n)}
            aria-label={t(key)}
            className={cn(
              'flex h-6 w-6 items-center justify-center rounded-full border text-xs font-medium',
              'border-primary bg-primary/20 text-primary',
              'cursor-pointer hover:bg-primary/30 transition-colors',
            )}
          >
            {n}
          </button>
        ) : (
          <div
            className={cn(
              'flex h-6 w-6 items-center justify-center rounded-full border text-xs font-medium',
              isActive && 'border-primary bg-primary text-primary-foreground',
              isDone && 'border-primary bg-primary/20 text-primary',
              !isActive && !isDone && 'border-input text-muted-foreground',
            )}
          >
            {n}
          </div>
        );
        return (
          <div key={n} className="flex items-center gap-2">
            {numberBox}
            <span
              className={cn(
                'text-xs',
                isActive ? 'font-medium text-foreground' : 'text-muted-foreground',
              )}
            >
              {t(key)}
            </span>
            {i < labels.length - 1 && <div className="mx-1 h-px w-4 bg-border" />}
          </div>
        );
      })}
    </div>
  );
}

