import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, ArrowLeft, ChevronDown } from 'lucide-react';
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
              <OAuthFieldset
                values={oauth}
                onChange={setOauth}
                collapsibleAdvanced
                flat
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

          {mode === 'headers' && (
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
              {mode === 'headers'
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
              {mode !== 'headers' && (
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

