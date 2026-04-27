import { useState, useCallback, useEffect, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { invalidateSetupStatusCache } from '@/router';
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
import { RequiredMark } from '@/components/ui/required-mark';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Check, Copy, Globe, ArrowRight, ArrowLeft, AlertTriangle, AlertCircle, Eye, EyeOff, Wand2 } from 'lucide-react';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { API_BASE, registerKeyPair } from '@/lib/api';

const STEPS = ['welcome', 'admin', 'settings', 'complete'] as const;
type Step = (typeof STEPS)[number];

interface SetupResult {
  admin_id: string;
  admin_email: string;
  api_key?: string;
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

// Module-scoped so each instance shares the compiled regex; recreating
// these per render adds up across the password-typing hot path.
const PASSWORD_HAS_UPPER = /[A-Z]/;
const PASSWORD_HAS_LOWER = /[a-z]/;
const PASSWORD_HAS_DIGIT = /\d/;

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
  const [passwordRevealed, setPasswordRevealed] = useState(false);

  // Settings form
  const [siteName, setSiteName] = useState('ThinkWatch');

  const [adminErrors, setAdminErrors] = useState<Record<string, string>>({});
  // Per-field touched state. `validateAdmin` always rebuilds the full
  // error map, but the render path gates visibility on `touched[field]`
  // so blurring the email input doesn't immediately flash "必填" under
  // every other field the user has yet to visit.
  const [adminTouched, setAdminTouched] = useState<Record<string, boolean>>({});

  // Hoisted out of the closure so the regex literals aren't recompiled
  // on every render (RegExp inside a body recompiles per call).
  const validateAdmin = useCallback((): boolean => {
    const errors: Record<string, string> = {};
    if (!email.trim()) errors.email = t('setup.validation.required');
    if (!displayName.trim()) errors.displayName = t('setup.validation.required');
    if (password.length < 8) {
      errors.password = t('setup.admin.passwordTooShort');
    } else if (
      !PASSWORD_HAS_UPPER.test(password) ||
      !PASSWORD_HAS_LOWER.test(password) ||
      !PASSWORD_HAS_DIGIT.test(password)
    ) {
      errors.password = t('setup.admin.passwordComplexity');
    }
    if (password !== confirmPassword) errors.confirmPassword = t('setup.admin.passwordMismatch');
    setAdminErrors(errors);
    return Object.keys(errors).length === 0;
  }, [email, displayName, password, confirmPassword, t]);

  const markAdminTouched = (field: string) => {
    setAdminTouched((prev) => (prev[field] ? prev : { ...prev, [field]: true }));
    validateAdmin();
  };

  // Generates a 16-char password that always satisfies the complexity
  // rules (≥1 upper, ≥1 lower, ≥1 digit). Reveals it inline so the
  // operator can copy it before continuing — generated → masked again
  // would be a footgun since they have no record of what was set.
  const handleGeneratePassword = () => {
    const upper = 'ABCDEFGHJKLMNPQRSTUVWXYZ';
    const lower = 'abcdefghijkmnopqrstuvwxyz';
    const digit = '23456789';
    const symbol = '!@#$%^&*-_=+';
    const all = upper + lower + digit + symbol;
    const rand = (alphabet: string) => {
      const buf = new Uint32Array(1);
      crypto.getRandomValues(buf);
      return alphabet[buf[0] % alphabet.length];
    };
    // Seed with one of each required class, then fill out and shuffle so
    // the required chars aren't always at fixed positions.
    const chars = [rand(upper), rand(lower), rand(digit), rand(symbol)];
    while (chars.length < 16) chars.push(rand(all));
    for (let i = chars.length - 1; i > 0; i--) {
      const buf = new Uint32Array(1);
      crypto.getRandomValues(buf);
      const j = buf[0] % (i + 1);
      [chars[i], chars[j]] = [chars[j], chars[i]];
    }
    const generated = chars.join('');
    setPassword(generated);
    setConfirmPassword(generated);
    setPasswordRevealed(true);
    setAdminTouched((prev) => ({ ...prev, password: true, confirmPassword: true }));
  };

  // Keep `adminErrors` in sync with the live form values. Without this,
  // after the user clicks Next on an empty form (which marks every
  // field touched), typing into a field fixes its value but the stale
  // "必填" error stays on screen until the user also blurs that field
  // — which looks like the validator is ignoring their input.
  // validateAdmin is useCallback-ed on [email, displayName, password,
  // confirmPassword, t] so this effect fires on every real change.
  useEffect(() => {
    validateAdmin();
  }, [validateAdmin]);

  const goNext = () => {
    const currentIndex = STEPS.indexOf(step);
    if (step === 'admin') {
      // Submit attempt: surface every error the user hasn't yet seen
      // by pretending they touched every field.
      setAdminTouched({
        email: true,
        displayName: true,
        password: true,
        confirmPassword: true,
      });
      if (!validateAdmin()) return;
    }
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

  const handleSubmit = async () => {
    setSubmitting(true);
    setError('');

    const body: Record<string, unknown> = {
      admin: { email, display_name: displayName, password },
    };
    if (siteName && siteName !== 'ThinkWatch') {
      body.site_name = siteName;
    }

    try {
      const res = await fetch(`${API_BASE}/api/setup/initialize`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: { message: res.statusText } }));
        throw new Error(err.error?.message ?? err.message ?? t('setup.error.generic'));
      }
      const data: SetupResult = await res.json();
      setResult(data);
      // Generate ECDSA key pair and register public key so the admin
      // is authenticated immediately after setup completes.
      await registerKeyPair();
      // Drop the cached setup status so the next router check re-fetches.
      // Without this the user has to hard-refresh after clicking
      // "go to login" because the cache still says needs_setup=true.
      invalidateSetupStatusCache();
      setStep('complete');
    } catch (err) {
      setError(err instanceof Error ? err.message : t('setup.error.generic'));
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
            {t('setup.languageLabel')}
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
        <Button data-primary-action className="w-full" onClick={goNext} size="lg">
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
          <Label htmlFor="setup-email">
            {t('setup.admin.email')} <RequiredMark />
          </Label>
          <Input
            id="setup-email"
            type="email"
            placeholder="admin@company.com"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            onBlur={() => markAdminTouched('email')}
            required
            aria-required="true"
            aria-invalid={adminTouched.email && !!adminErrors.email}
          />
          {adminTouched.email && adminErrors.email && (
            <p className="text-xs text-destructive">{adminErrors.email}</p>
          )}
        </div>
        <div className="space-y-2">
          <Label htmlFor="setup-display-name">
            {t('setup.admin.displayName')} <RequiredMark />
          </Label>
          <Input
            id="setup-display-name"
            placeholder="Admin"
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
            onBlur={() => markAdminTouched('displayName')}
            required
            aria-required="true"
            aria-invalid={adminTouched.displayName && !!adminErrors.displayName}
          />
          {adminTouched.displayName && adminErrors.displayName && (
            <p className="text-xs text-destructive">{adminErrors.displayName}</p>
          )}
        </div>
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <Label htmlFor="setup-password">
              {t('setup.admin.password')} <RequiredMark />
            </Label>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-xs"
              onClick={handleGeneratePassword}
            >
              <Wand2 className="mr-1 h-3 w-3" />
              {t('setup.admin.generatePassword')}
            </Button>
          </div>
          <div className="relative">
            <Input
              id="setup-password"
              type={passwordRevealed ? 'text' : 'password'}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              onBlur={() => markAdminTouched('password')}
              required
              aria-required="true"
              aria-invalid={adminTouched.password && !!adminErrors.password}
              className="pr-9"
            />
            <button
              type="button"
              onClick={() => setPasswordRevealed((v) => !v)}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
              aria-label={passwordRevealed ? t('common.hide') : t('common.show')}
            >
              {passwordRevealed ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
            </button>
          </div>
          <p className="text-xs text-muted-foreground">
            {t('setup.admin.passwordHint')}
          </p>
          {adminTouched.password && adminErrors.password && (
            <p className="text-xs text-destructive">{adminErrors.password}</p>
          )}
        </div>
        <div className="space-y-2">
          <Label htmlFor="setup-confirm-password">
            {t('setup.admin.confirmPassword')} <RequiredMark />
          </Label>
          <Input
            id="setup-confirm-password"
            type="password"
            value={confirmPassword}
            onChange={(e) => setConfirmPassword(e.target.value)}
            onBlur={() => markAdminTouched('confirmPassword')}
            required
            aria-required="true"
            aria-invalid={adminTouched.confirmPassword && !!adminErrors.confirmPassword}
          />
          {adminTouched.confirmPassword && adminErrors.confirmPassword && (
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
          {t('setup.complete.goToDashboard')}
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
      case 'complete':
        return renderComplete();
    }
  };

  const handleFormSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (step === 'settings') {
      handleSubmit();
    } else if (step === 'admin') {
      goNext();
    }
  };

  // Enter should advance the wizard regardless of focus — the default
  // form-submit path only fires while an input is focused, so a user
  // who clicked the card background or tabbed to a label would see
  // Enter do nothing. Dispatch via a DOM-level `.click()` on the
  // button tagged `data-primary-action` instead of calling goNext /
  // handleSubmit from the listener directly: the listener has to
  // survive across renders without its closures going stale on the
  // current step's validation state, and React attaches the
  // always-fresh click handler to the rendered button for us.
  // `isComposing` guards against swallowing an IME commit.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Enter' || e.isComposing || e.shiftKey) return;
      // Let explicit button activation through — Space / Enter on a
      // focused <button> already does the right thing, and a textarea
      // legitimately wants Enter as a newline.
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === 'TEXTAREA' || target.tagName === 'BUTTON')) {
        return;
      }
      const primary = document.querySelector<HTMLButtonElement>(
        'button[data-primary-action]:not([disabled])',
      );
      if (primary) {
        e.preventDefault();
        primary.click();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const showBack = step !== 'welcome' && step !== 'complete';
  const showNext = step === 'admin';
  const showSubmit = step === 'settings';

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
            {(showBack || showNext || showSubmit) && (
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
                  {showNext && (
                    <Button data-primary-action type="submit">
                      {t('setup.next')}
                      <ArrowRight className="ml-2 h-4 w-4" />
                    </Button>
                  )}
                  {showSubmit && (
                    <Button data-primary-action type="submit" disabled={submitting}>
                      {submitting ? t('common.loading') : t('setup.complete.cta')}
                      <ArrowRight className="ml-2 h-4 w-4" />
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
