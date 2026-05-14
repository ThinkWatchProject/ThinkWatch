import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { AlertCircle } from 'lucide-react';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { API_BASE } from '@/lib/api';
import { useSsoStatus } from '@/hooks/use-sso-status';
import { usePowChallenge, type PowSolution } from '@/hooks/use-pow-challenge';
import { PowIndicator } from '@/components/auth/pow-indicator';

interface LoginPageProps {
  onLogin: (
    email: string,
    password: string,
    totpCode?: string,
    pow?: PowSolution,
  ) => Promise<{ totp_required?: boolean; password_change_required?: boolean }>;
}

export function LoginPage({ onLogin }: LoginPageProps) {
  const { t } = useTranslation();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const { ssoEnabled, allowRegistration: registrationOpen } = useSsoStatus();
  const [totpStep, setTotpStep] = useState(false);
  const [totpCode, setTotpCode] = useState('');
  // PoW grinder runs in a Web Worker from mount. By the time the
  // user has typed their password and clicked Sign in, the solution
  // is usually already `ready` and the click is instant.
  const pow = usePowChallenge();

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError('');
    if (!pow.solution) {
      // Worker still grinding (slow device or just-mounted page).
      // The Sign-in button is disabled in that state, but defend
      // against a programmatic submit.
      setError(t('auth.powStillVerifying'));
      return;
    }
    setLoading(true);
    try {
      const res = await onLogin(
        email,
        password,
        totpStep ? totpCode : undefined,
        pow.solution,
      );
      if (res.totp_required) {
        setTotpStep(true);
        // TOTP step also needs a fresh PoW (the previous one was
        // consumed by the password call). Mint + grind in the
        // background so the user can paste their code while we work.
        pow.refresh();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
      // Failed login consumed the PoW; mint a new one so the next
      // attempt isn't artificially delayed.
      pow.refresh();
    } finally {
      setLoading(false);
    }
  };

  const handleSsoLogin = () => {
    window.location.href = `${API_BASE}/api/auth/sso/authorize`;
  };

  // Disable submit while the worker hasn't produced a solution.
  // `error` PoW state means the challenge endpoint failed (rate
  // limit or backend down) — surface that as a recoverable error
  // rather than silently locking the form.
  const submitDisabled = loading || pow.status !== 'ready';

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <ThinkWatchMark className="h-7 w-7" />
          </div>
          <CardTitle className="text-2xl">{t('auth.title')}</CardTitle>
          <CardDescription>{t('auth.subtitle')}</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit} className="space-y-4">
            {error && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label htmlFor="email">{t('auth.email')}</Label>
              <Input
                id="email"
                type="email"
                placeholder="admin@company.com"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="password">{t('auth.password')}</Label>
              <Input
                id="password"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
                disabled={totpStep}
              />
            </div>
            {totpStep && (
              <div className="space-y-2">
                <Label htmlFor="totp">{t('auth.totpCode')}</Label>
                <Input
                  id="totp"
                  type="text"
                  inputMode="numeric"
                  pattern="[0-9A-Z\-]*"
                  maxLength={10}
                  placeholder="000000"
                  value={totpCode}
                  onChange={(e) => setTotpCode(e.target.value.toUpperCase())}
                  autoFocus
                  required
                />
                <p className="text-xs text-muted-foreground">{t('auth.totpHint')}</p>
              </div>
            )}
            <Button type="submit" className="w-full" disabled={submitDisabled}>
              {loading ? t('auth.signingIn') : t('auth.signIn')}
            </Button>
            {/* Always-visible PoW status. Renders in every state
                (fetching / grinding / ready / error) so the user
                sees the security work happening — no mysterious
                button-disabled period and no spec-detail noise
                hidden inside the button label. */}
            <PowIndicator
              status={pow.status}
              tried={pow.tried}
              elapsedMs={pow.elapsedMs}
              difficulty={pow.difficulty}
              errorMessage={pow.error}
              onRetry={pow.refresh}
            />
            {ssoEnabled && (
              <>
                <div className="relative my-4">
                  <div className="absolute inset-0 flex items-center">
                    <span className="w-full border-t" />
                  </div>
                  <div className="relative flex justify-center text-xs uppercase">
                    <span className="bg-card px-2 text-muted-foreground">{t('auth.or')}</span>
                  </div>
                </div>
                <Button type="button" variant="outline" className="w-full" onClick={handleSsoLogin}>
                  {t('auth.signInWith')}
                </Button>
              </>
            )}
            {registrationOpen && (
              <div className="text-center">
                <a href="/register" className="text-sm text-muted-foreground hover:text-foreground">
                  {t('auth.noAccount')}
                </a>
              </div>
            )}
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
