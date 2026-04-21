import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from '@tanstack/react-router';
import { X, KeyRound, Plug, Users } from 'lucide-react';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { hasPermission } from '@/lib/api';

const DISMISSED_KEY = 'dashboard.gettingStarted.dismissed';

/**
 * Inline onboarding card that surfaces the three actions a freshly-
 * provisioned admin needs to do next: mint an API key, configure a
 * provider, invite collaborators. Hidden once any of the conditions
 * is met (no permission to act, or the user has dismissed the card).
 *
 * `signals` is the lightweight summary the page already loads — we
 * use it to suppress the card automatically when the platform has
 * graduated past first-run state.
 */
export function GettingStartedCard({
  signals,
}: {
  signals: { hasApiKeys: boolean; hasProviders: boolean };
}) {
  const { t } = useTranslation();
  const [dismissed, setDismissed] = useState(false);
  useEffect(() => {
    try {
      if (window.localStorage.getItem(DISMISSED_KEY) === '1') setDismissed(true);
    } catch {
      // ignore
    }
  }, []);

  if (dismissed) return null;
  // Auto-suppress once the platform is past first-run state — the
  // card is for the empty dashboard, not a recurring nudge.
  if (signals.hasApiKeys && signals.hasProviders) return null;

  const dismiss = () => {
    setDismissed(true);
    try {
      window.localStorage.setItem(DISMISSED_KEY, '1');
    } catch {
      // ignore
    }
  };

  return (
    <Card className="relative bg-muted/30 border-dashed">
      <CardContent className="py-4">
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={dismiss}
          aria-label={t('common.dismiss')}
          className="absolute right-2 top-2"
        >
          <X className="h-4 w-4" />
        </Button>
        <div className="mb-3">
          <h2 className="text-sm font-semibold">{t('dashboard.gettingStarted.title')}</h2>
          <p className="text-xs text-muted-foreground">
            {t('dashboard.gettingStarted.subtitle')}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {!signals.hasApiKeys && hasPermission('api_keys:create') && (
            <Link to="/api-keys">
              <Button variant="outline" size="sm">
                <KeyRound className="h-4 w-4" />
                {t('dashboard.gettingStarted.createKey')}
              </Button>
            </Link>
          )}
          {!signals.hasProviders && hasPermission('providers:create') && (
            <Link to="/gateway/providers">
              <Button variant="outline" size="sm">
                <Plug className="h-4 w-4" />
                {t('dashboard.gettingStarted.addProvider')}
              </Button>
            </Link>
          )}
          {hasPermission('users:create') && (
            <Link to="/admin/users">
              <Button variant="outline" size="sm">
                <Users className="h-4 w-4" />
                {t('dashboard.gettingStarted.inviteUsers')}
              </Button>
            </Link>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
