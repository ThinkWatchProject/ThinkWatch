import { ShieldX } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Link } from '@tanstack/react-router';

import { hasPermission } from '@/lib/api';
import { Button } from '@/components/ui/button';

interface RequirePermissionProps {
  /**
   * Permission string the route's main API call requires (e.g.
   * `users:read`). `undefined` is allowed for routes any
   * authenticated user can hit — the wrapper still renders, but
   * the gate is a no-op. Keeps the router definition uniform.
   */
  perm: string | undefined;
  children: React.ReactNode;
}

/**
 * Route-level permission gate. The backend is the authoritative
 * security boundary (every protected handler does
 * `auth_user.require_permission(...)`); this wrapper exists for
 * UX so a developer who types `/admin/users` directly sees a
 * proper 403 page instead of a half-rendered admin shell firing
 * 403s on every API call.
 *
 * The check uses the cached permissions list `/api/auth/me` shipped
 * back at login — see `hasPermission()` in `lib/api.ts`. Since the
 * cache fills before the router mounts (auth bootstrap blocks the
 * first paint), there's no flash-of-permitted-content here.
 */
export function RequirePermission({ perm, children }: RequirePermissionProps) {
  if (!perm || hasPermission(perm)) {
    return <>{children}</>;
  }
  return <ForbiddenPage perm={perm} />;
}

/**
 * Shared 403 surface. Friendlier than a toast on top of an empty
 * table — explains what went wrong, surfaces the missing permission
 * for support tickets, and offers a way back.
 */
export function ForbiddenPage({ perm }: { perm?: string }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-center justify-center min-h-[60vh] gap-4 px-6 text-center">
      <ShieldX
        className="size-16 text-muted-foreground"
        aria-hidden="true"
      />
      <h1 className="text-2xl font-semibold tracking-tight">
        {t('forbidden.title')}
      </h1>
      <p className="max-w-md text-muted-foreground">{t('forbidden.message')}</p>
      {perm && (
        <p className="text-xs text-muted-foreground font-mono">
          {t('forbidden.requiredPermission')}:{' '}
          <span className="px-1.5 py-0.5 rounded bg-muted">{perm}</span>
        </p>
      )}
      <Button asChild variant="default" className="mt-2">
        <Link to="/">{t('forbidden.backToDashboard')}</Link>
      </Button>
    </div>
  );
}
