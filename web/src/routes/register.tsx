import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Shield, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { apiPost, setCachedPermissions } from '@/lib/api';

interface RegisterPageProps {
  onRegistered: (signingKey: string) => void;
}

interface RegisterResponse {
  signing_key: string;
  permissions?: string[];
}

export function RegisterPage({ onRegistered }: RegisterPageProps) {
  const { t } = useTranslation();
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError('');
    if (password !== confirmPassword) {
      setError(t('auth.passwordMismatch'));
      return;
    }
    if (password.length < 8) {
      setError(t('auth.passwordTooShort'));
      return;
    }
    if (!/[A-Z]/.test(password) || !/[a-z]/.test(password) || !/\d/.test(password)) {
      setError(t('auth.passwordComplexity'));
      return;
    }
    setLoading(true);
    try {
      const res = await apiPost<RegisterResponse>('/api/auth/register', {
        email,
        display_name: displayName,
        password,
      });
      if (!res.signing_key) {
        // Server returns 200 with empty signing_key for both success
        // and duplicate-email to prevent user enumeration. Show a
        // generic message so the user knows to try logging in.
        setError(t('auth.registrationFailed'));
        return;
      }
      sessionStorage.setItem('signing_key', res.signing_key);
      setCachedPermissions(res.permissions);
      onRegistered(res.signing_key);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Registration failed');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <Shield className="h-6 w-6" />
          </div>
          <CardTitle className="text-2xl">{t('auth.registerTitle')}</CardTitle>
          <CardDescription>{t('auth.registerSubtitle')}</CardDescription>
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
              <Label htmlFor="reg-email">{t('auth.email')}</Label>
              <Input
                id="reg-email"
                type="email"
                placeholder="you@company.com"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="reg-name">{t('auth.displayName')}</Label>
              <Input
                id="reg-name"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                placeholder="John Doe"
                required
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="reg-password">{t('auth.password')}</Label>
              <Input
                id="reg-password"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="reg-confirm">{t('auth.confirmPassword')}</Label>
              <Input
                id="reg-confirm"
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                required
              />
            </div>
            <Button type="submit" className="w-full" disabled={loading}>
              {loading ? t('auth.registering') : t('auth.register')}
            </Button>
            <div className="text-center">
              <a href="/" className="text-sm text-muted-foreground hover:text-foreground">
                {t('auth.haveAccount')}
              </a>
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
