import { useTranslation } from 'react-i18next';
import { LogOut } from 'lucide-react';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';
import { TotpEnrollment } from '@/components/auth/totp-enrollment';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';

/**
 * Shown instead of the console while the platform requires TOTP and the
 * signed-in user has not enrolled. The server refuses every other console
 * request for this session until enrollment completes, so nothing else
 * is reachable from here but signing out.
 */
export function TotpEnrollmentPage({
  email,
  onEnrolled,
  onLogout,
}: {
  email: string;
  onEnrolled: () => void | Promise<void>;
  onLogout: () => void | Promise<void>;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <ThinkWatchMark className="h-7 w-7" />
          </div>
          <CardTitle className="text-2xl">{t('auth.totpEnrollmentTitle')}</CardTitle>
          <CardDescription>{t('auth.totpEnrollmentDescription')}</CardDescription>
        </CardHeader>
        <CardContent className="space-y-6">
          <TotpEnrollment onEnrolled={onEnrolled} />
          <div className="flex items-center justify-between gap-2 border-t pt-4 text-sm text-muted-foreground">
            <span className="truncate">{t('auth.totpEnrollmentSignedInAs', { email })}</span>
            <Button variant="ghost" size="sm" onClick={() => void onLogout()}>
              <LogOut className="h-3.5 w-3.5" />
              {t('auth.logout')}
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
