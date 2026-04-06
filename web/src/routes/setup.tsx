import { useState, useCallback, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Check, Copy, Globe, ArrowRight, ArrowLeft, AlertTriangle, AlertCircle, Plus, X } from 'lucide-react';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';
import { Alert, AlertDescription } from '@/components/ui/alert';

const API_BASE = import.meta.env.VITE_API_BASE ?? '';

const STEPS = ['welcome', 'admin', 'settings', 'provider', 'complete'] as const;
type Step = (typeof STEPS)[number];

const PROVIDER_TYPES = [
  { value: 'openai', label: 'OpenAI', baseUrl: 'https://api.openai.com' },
  { value: 'anthropic', label: 'Anthropic', baseUrl: 'https://api.anthropic.com' },
  { value: 'google', label: 'Google Gemini', baseUrl: 'https://generativelanguage.googleapis.com' },
  { value: 'azure_openai', label: 'Azure OpenAI', baseUrl: '' },
  { value: 'bedrock', label: 'AWS Bedrock', baseUrl: 'us-east-1' },
  { value: 'custom', label: 'Custom (OpenAI-compatible)', baseUrl: '' },
];

interface SetupResult {
  admin_id: string;
  admin_email: string;
  api_key?: string;
  provider_id?: string;
  message: string;
}

function StepIndicator({ currentStep }: { currentStep: Step }) {
  const { t } = useTranslation();
  const currentIndex = STEPS.indexOf(currentStep);

  return (
    <div className="flex items-center justify-center gap-2 mb-8">
      {STEPS.map((step, index) => {
        const isCompleted = index < currentIndex;
        const isCurrent = index === currentIndex;

        return (
          <div key={step} className="flex items-center gap-2">
            <div className="flex flex-col items-center gap-1">
              <div
                className={`flex h-8 w-8 items-center justify-center rounded-full text-xs font-medium transition-colors ${
                  isCompleted
                    ? 'bg-primary text-primary-foreground'
                    : isCurrent
                      ? 'bg-primary text-primary-foreground'
                      : 'bg-muted text-muted-foreground'
                }`}
              >
                {isCompleted ? <Check className="h-4 w-4" /> : index + 1}
              </div>
              <span
                className={`text-xs hidden sm:block ${
                  isCurrent ? 'text-foreground font-medium' : 'text-muted-foreground'
                }`}
              >
                {t(`setup.steps.${step}`)}
              </span>
            </div>
            {index < STEPS.length - 1 && (
              <div
                className={`h-px w-6 sm:w-10 mt-[-1.25rem] sm:mt-[-1.25rem] ${
                  isCompleted ? 'bg-primary' : 'bg-muted'
                }`}
              />
            )}
          </div>
        );
      })}
    </div>
  );
}

export function SetupPage() {
  const { t, i18n } = useTranslation();
  const [step, setStep] = useState<Step>('welcome');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');
  const [result, setResult] = useState<SetupResult | null>(null);
  const [copied, setCopied] = useState(false);

  // Admin form
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');

  // Settings form
  const [siteName, setSiteName] = useState('ThinkWatch');

  // Provider form
  const [providerType, setProviderType] = useState('');
  const [providerName, setProviderName] = useState('');
  const [providerDisplayName, setProviderDisplayName] = useState('');
  const [providerBaseUrl, setProviderBaseUrl] = useState('');
  const [providerApiKey, setProviderApiKey] = useState('');
  const [providerHeaders, setProviderHeaders] = useState<[string, string][]>([]);
  // Provider connection test state
  const [providerTesting, setProviderTesting] = useState(false);
  const [providerTestResult, setProviderTestResult] = useState<{
    success: boolean;
    message: string;
    latency_ms: number;
  } | null>(null);

  const [adminErrors, setAdminErrors] = useState<Record<string, string>>({});

  const validateAdmin = useCallback((): boolean => {
    const errors: Record<string, string> = {};
    if (!email.trim()) errors.email = 'Required';
    if (!displayName.trim()) errors.displayName = 'Required';
    if (password.length < 8) {
      errors.password = t('setup.admin.passwordTooShort');
    } else if (
      !/[A-Z]/.test(password) ||
      !/[a-z]/.test(password) ||
      !/\d/.test(password)
    ) {
      errors.password = t('setup.admin.passwordComplexity');
    }
    if (password !== confirmPassword) errors.confirmPassword = t('setup.admin.passwordMismatch');
    setAdminErrors(errors);
    return Object.keys(errors).length === 0;
  }, [email, displayName, password, confirmPassword, t]);

  const goNext = () => {
    const currentIndex = STEPS.indexOf(step);
    if (step === 'admin' && !validateAdmin()) return;
    if (currentIndex < STEPS.length - 1) {
      setError('');
      setStep(STEPS[currentIndex + 1]);
    }
  };

  const goBack = () => {
    const currentIndex = STEPS.indexOf(step);
    if (currentIndex > 0) {
      setError('');
      setStep(STEPS[currentIndex - 1]);
    }
  };

  const handleTestProvider = async () => {
    setProviderTesting(true);
    setProviderTestResult(null);
    try {
      const customHeaders: Record<string, string> = {};
      for (const [k, v] of providerHeaders) {
        if (k && v) customHeaders[k] = v;
      }
      const res = await fetch(`${API_BASE}/api/setup/test-provider`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_type: providerType,
          base_url: providerBaseUrl,
          api_key: providerApiKey,
          custom_headers: Object.keys(customHeaders).length > 0 ? customHeaders : undefined,
        }),
      });
      const data = await res.json();
      if (!res.ok) {
        setProviderTestResult({
          success: false,
          message: data.error?.message || `HTTP ${res.status}`,
          latency_ms: 0,
        });
      } else {
        setProviderTestResult(data);
      }
    } catch (err) {
      setProviderTestResult({
        success: false,
        message: err instanceof Error ? err.message : 'Network error',
        latency_ms: 0,
      });
    } finally {
      setProviderTesting(false);
    }
  };

  const handleProviderTypeChange = (value: string | null) => {
    if (value === null) return;
    setProviderType(value);
    setProviderTestResult(null); // reset test status when type changes
    const found = PROVIDER_TYPES.find((pt) => pt.value === value);
    if (found) {
      setProviderBaseUrl(found.baseUrl);
      if (!providerName) setProviderName(value);
      if (!providerDisplayName) setProviderDisplayName(found.label);
    }
  };

  const handleSubmit = async (skipProvider = false) => {
    setSubmitting(true);
    setError('');

    const body: Record<string, unknown> = {
      admin: { email, display_name: displayName, password },
    };
    if (siteName && siteName !== 'ThinkWatch') {
      body.site_name = siteName;
    }
    if (!skipProvider && providerType && providerName && providerBaseUrl && providerApiKey) {
      body.provider = {
        name: providerName,
        display_name: providerDisplayName || providerName,
        provider_type: providerType,
        base_url: providerBaseUrl,
        api_key: providerApiKey,
        custom_headers: Object.fromEntries(providerHeaders.filter(([k, v]) => k && v)),
      };
    }

    try {
      const res = await fetch(`${API_BASE}/api/setup/initialize`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: { message: res.statusText } }));
        throw new Error(err.error?.message ?? err.message ?? 'Setup failed');
      }
      const data: SetupResult = await res.json();
      setResult(data);
      setStep('complete');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Setup failed');
    } finally {
      setSubmitting(false);
    }
  };

  const handleCopyApiKey = async () => {
    if (result?.api_key) {
      await navigator.clipboard.writeText(result.api_key);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  };

  const changeLanguage = (value: string | null) => {
    if (value) i18n.changeLanguage(value);
  };

  const renderWelcome = () => (
    <>
      <CardHeader className="text-center">
        <div className="mx-auto mb-4 flex h-14 w-14 items-center justify-center rounded-xl bg-primary text-primary-foreground">
          <ThinkWatchMark className="h-9 w-9" />
        </div>
        <CardTitle className="text-2xl">{t('setup.title')}</CardTitle>
        <CardDescription>{t('setup.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        <div className="space-y-2">
          <Label className="flex items-center gap-2">
            <Globe className="h-4 w-4" />
            Language / 语言
          </Label>
          <Select value={i18n.language} onValueChange={changeLanguage}>
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="en">English</SelectItem>
              <SelectItem value="zh">中文</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <Button className="w-full" onClick={goNext} size="lg">
          {t('setup.getStarted')}
          <ArrowRight className="ml-2 h-4 w-4" />
        </Button>
      </CardContent>
    </>
  );

  const renderAdmin = () => (
    <>
      <CardHeader className="text-center">
        <CardTitle>{t('setup.admin.title')}</CardTitle>
        <CardDescription>{t('setup.admin.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-2">
          <Label htmlFor="setup-email">{t('setup.admin.email')}</Label>
          <Input
            id="setup-email"
            type="email"
            placeholder="admin@company.com"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
          {adminErrors.email && (
            <p className="text-xs text-destructive">{adminErrors.email}</p>
          )}
        </div>
        <div className="space-y-2">
          <Label htmlFor="setup-display-name">{t('setup.admin.displayName')}</Label>
          <Input
            id="setup-display-name"
            placeholder="Admin"
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
            required
          />
          {adminErrors.displayName && (
            <p className="text-xs text-destructive">{adminErrors.displayName}</p>
          )}
        </div>
        <div className="space-y-2">
          <Label htmlFor="setup-password">{t('setup.admin.password')}</Label>
          <Input
            id="setup-password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
          <p className="text-xs text-muted-foreground">
            {t('setup.admin.passwordHint')}
          </p>
          {adminErrors.password && (
            <p className="text-xs text-destructive">{adminErrors.password}</p>
          )}
        </div>
        <div className="space-y-2">
          <Label htmlFor="setup-confirm-password">{t('setup.admin.confirmPassword')}</Label>
          <Input
            id="setup-confirm-password"
            type="password"
            value={confirmPassword}
            onChange={(e) => setConfirmPassword(e.target.value)}
            required
          />
          {adminErrors.confirmPassword && (
            <p className="text-xs text-destructive">{adminErrors.confirmPassword}</p>
          )}
        </div>
      </CardContent>
    </>
  );

  const renderSettings = () => (
    <>
      <CardHeader className="text-center">
        <CardTitle>{t('setup.settings.title')}</CardTitle>
        <CardDescription>{t('setup.settings.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-2">
          <Label htmlFor="setup-site-name">{t('setup.settings.siteName')}</Label>
          <Input
            id="setup-site-name"
            value={siteName}
            onChange={(e) => setSiteName(e.target.value)}
          />
        </div>
      </CardContent>
    </>
  );

  const renderProvider = () => (
    <>
      <CardHeader className="text-center">
        <CardTitle>{t('setup.provider.title')}</CardTitle>
        <CardDescription>{t('setup.provider.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-2">
          <Label>{t('setup.provider.providerType')}</Label>
          <Select value={providerType} onValueChange={handleProviderTypeChange}>
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PROVIDER_TYPES.map((pt) => (
                <SelectItem key={pt.value} value={pt.value}>
                  {pt.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        {providerType && (
          <>
            <div className="space-y-2">
              <Label htmlFor="setup-provider-name">{t('setup.provider.name')}</Label>
              <Input
                id="setup-provider-name"
                value={providerName}
                onChange={(e) => setProviderName(e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="setup-provider-display-name">{t('setup.provider.displayName')}</Label>
              <Input
                id="setup-provider-display-name"
                value={providerDisplayName}
                onChange={(e) => setProviderDisplayName(e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="setup-provider-base-url">{t('setup.provider.baseUrl')}</Label>
              <Input
                id="setup-provider-base-url"
                value={providerBaseUrl}
                onChange={(e) => setProviderBaseUrl(e.target.value)}
                placeholder={
                  providerType === 'azure_openai' ? 'https://your-resource.openai.azure.com' :
                  providerType === 'bedrock' ? 'us-east-1' :
                  providerType === 'anthropic' ? 'https://api.anthropic.com' :
                  providerType === 'google' ? 'https://generativelanguage.googleapis.com' :
                  'https://api.openai.com'
                }
              />
              {providerType === 'bedrock' && (
                <p className="text-xs text-muted-foreground">{t('providers.bedrockUrlHint', 'Enter AWS region (e.g. us-east-1)')}</p>
              )}
              {providerType === 'azure_openai' && (
                <p className="text-xs text-muted-foreground">{t('providers.azureUrlHint', 'Enter Azure OpenAI resource endpoint')}</p>
              )}
            </div>
            <div className="space-y-2">
              <Label htmlFor="setup-provider-api-key">{providerType === 'bedrock' ? t('providers.awsCredentials', 'AWS Credentials') : t('setup.provider.apiKey')}</Label>
              <Input
                id="setup-provider-api-key"
                type="password"
                value={providerApiKey}
                onChange={(e) => setProviderApiKey(e.target.value)}
                placeholder={providerType === 'bedrock' ? 'ACCESS_KEY_ID:SECRET_ACCESS_KEY' : 'sk-...'}
              />
              {providerType === 'bedrock' && (
                <p className="text-xs text-muted-foreground">{t('providers.bedrockKeyHint', 'Format: ACCESS_KEY_ID:SECRET_ACCESS_KEY')}</p>
              )}
            </div>
            <div className="space-y-2">
              <Label>{t('providers.customHeaders')}</Label>
              <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
              {providerHeaders.map(([k, v], i) => (
                <div key={i} className="flex gap-2 items-center">
                  <Input className="flex-1" placeholder="Header-Name" value={k}
                    onChange={(e) => { const next = [...providerHeaders]; next[i] = [e.target.value, v]; setProviderHeaders(next); }} />
                  <Input className="flex-1" placeholder="value" value={v}
                    onChange={(e) => { const next = [...providerHeaders]; next[i] = [k, e.target.value]; setProviderHeaders(next); }} />
                  <Button type="button" variant="ghost" size="icon-sm" onClick={() => setProviderHeaders(providerHeaders.filter((_, j) => j !== i))}>
                    <X className="h-3 w-3" />
                  </Button>
                </div>
              ))}
              <Button type="button" variant="outline" size="sm" onClick={() => setProviderHeaders([...providerHeaders, ['', '']])}>
                <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
              </Button>
            </div>

            {/* Test connection */}
            <div className="space-y-2 pt-2 border-t">
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={providerTesting || !providerBaseUrl || !providerApiKey}
                onClick={handleTestProvider}
              >
                {providerTesting ? t('common.loading') : t('setup.provider.testConnection')}
              </Button>
              {providerTestResult && (
                <Alert variant={providerTestResult.success ? 'default' : 'destructive'}>
                  {providerTestResult.success ? (
                    <Check className="h-4 w-4" />
                  ) : (
                    <AlertCircle className="h-4 w-4" />
                  )}
                  <AlertDescription>
                    {providerTestResult.message}
                    {providerTestResult.latency_ms > 0 && (
                      <span className="text-xs text-muted-foreground ml-2">
                        ({providerTestResult.latency_ms}ms)
                      </span>
                    )}
                  </AlertDescription>
                </Alert>
              )}
            </div>
          </>
        )}
      </CardContent>
    </>
  );

  const renderComplete = () => (
    <>
      <CardHeader className="text-center">
        <div className="mx-auto mb-4 flex h-14 w-14 items-center justify-center rounded-xl bg-green-100 text-green-600 dark:bg-green-900/30 dark:text-green-400">
          <Check className="h-7 w-7" />
        </div>
        <CardTitle className="text-2xl">{t('setup.complete.title')}</CardTitle>
        <CardDescription>{t('setup.complete.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {result?.api_key && (
          <div className="space-y-3">
            <Label>{t('setup.complete.apiKeyLabel')}</Label>
            <div className="flex gap-2">
              <Input
                readOnly
                value={result.api_key}
                className="font-mono text-sm"
              />
              <Button variant="outline" size="icon" onClick={handleCopyApiKey}>
                {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
              </Button>
            </div>
            <div className="flex items-start gap-2 rounded-md bg-amber-50 p-3 text-sm text-amber-800 dark:bg-amber-950/30 dark:text-amber-300">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{t('setup.complete.apiKeyWarning')}</span>
            </div>
          </div>
        )}
        <Button
          className="w-full"
          size="lg"
          onClick={() => { window.location.href = '/'; }}
        >
          {t('setup.complete.goToLogin')}
          <ArrowRight className="ml-2 h-4 w-4" />
        </Button>
      </CardContent>
    </>
  );

  const renderStepContent = () => {
    switch (step) {
      case 'welcome':
        return renderWelcome();
      case 'admin':
        return renderAdmin();
      case 'settings':
        return renderSettings();
      case 'provider':
        return renderProvider();
      case 'complete':
        return renderComplete();
    }
  };

  const handleFormSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (step === 'provider') {
      handleSubmit(false);
    } else if (step !== 'complete' && step !== 'welcome') {
      goNext();
    }
  };

  const showBack = step !== 'welcome' && step !== 'complete';
  const showNext = step === 'admin' || step === 'settings';
  const showSubmit = step === 'provider';
  const showSkip = step === 'provider';

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <div className="w-full max-w-lg">
        {step !== 'welcome' && <StepIndicator currentStep={step} />}
        <Card>
          <form onSubmit={handleFormSubmit}>
            {error && (
              <div className="mx-6 mt-6">
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              </div>
            )}
            {renderStepContent()}
            {(showBack || showNext || showSubmit || showSkip) && (
              <div className="flex items-center justify-between px-6 pb-6 pt-2">
                <div>
                  {showBack && (
                    <Button type="button" variant="ghost" onClick={goBack}>
                      <ArrowLeft className="mr-2 h-4 w-4" />
                      {t('setup.back')}
                    </Button>
                  )}
                </div>
                <div className="flex gap-2">
                  {showSkip && (
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => handleSubmit(true)}
                      disabled={submitting}
                    >
                      {t('setup.provider.skip')}
                    </Button>
                  )}
                  {showNext && (
                    <Button type="submit">
                      {t('setup.next')}
                      <ArrowRight className="ml-2 h-4 w-4" />
                    </Button>
                  )}
                  {showSubmit && (
                    <Button type="submit" disabled={submitting}>
                      {submitting ? t('common.loading') : t('setup.next')}
                    </Button>
                  )}
                </div>
              </div>
            )}
          </form>
        </Card>
      </div>
    </div>
  );
}
