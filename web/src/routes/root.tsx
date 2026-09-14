import { useEffect, useState } from 'react';
import { Outlet, useNavigate, useRouterState } from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { ErrorBoundary } from '@/components/error-boundary';
import { CommandPalette } from '@/components/command-palette';
import { AppShell } from '@/components/layout/app-shell';
import { useAuth } from '@/hooks/use-auth';
import { useSsoStatus } from '@/hooks/use-sso-status';
import { API_BASE } from '@/lib/api';
import { SetupStatusSchema } from '@/lib/schemas';
import { readSetupStatus, rememberSetupStatus } from '@/lib/setup-status';
import { LoginPage } from '@/routes/login';
import { SetupPage } from '@/routes/setup';

// Split out of `router.tsx`: that module has to export the route tree, and a
// module exporting both components and plain values loses Fast Refresh.

export function RootComponent() {
  const { t } = useTranslation();
  const { user, loading, login, logout, handleSsoCallback } = useAuth();
  const [setupChecked, setSetupChecked] = useState(readSetupStatus() !== null);
  const [needsSetup, setNeedsSetup] = useState(readSetupStatus()?.needs_setup ?? false);
  const { allowRegistration: registrationOpen } = useSsoStatus();
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  // Check setup status on mount AND when the tab becomes visible — the
  // latter handles the "user completed setup in another tab" case.
  useEffect(() => {
    let cancelled = false;
    const check = () => {
      if (cancelled) return;
      fetch(`${API_BASE}/api/setup/status`)
        .then((r) => r.json())
        .then((raw) => {
          const data = SetupStatusSchema.parse(raw);
          if (cancelled) return;
          rememberSetupStatus(data);
          setNeedsSetup(data.needs_setup);
          setSetupChecked(true);
        })
        .catch(() => {
          if (cancelled) return;
          rememberSetupStatus({ initialized: true, needs_setup: false });
          setSetupChecked(true);
        });
    };
    if (readSetupStatus() === null) check();
    const onVis = () => {
      // When the tab becomes visible, re-check IF the cache was invalidated
      // (or if we're still in needs_setup state — covers the case where the
      // user just finished setup in this tab).
      if (!document.hidden && (readSetupStatus() === null || readSetupStatus()!.needs_setup)) {
        check();
      }
    };
    document.addEventListener('visibilitychange', onVis);
    return () => {
      cancelled = true;
      document.removeEventListener('visibilitychange', onVis);
    };
  }, []);

  // Handle SSO callback. Auth cookies were set on the redirect
  // response; the fragment just signals that SSO completed. The
  // client generates an ECDSA key pair and registers the public
  // key with the server.
  useEffect(() => {
    const hash = window.location.hash;
    if (hash.includes('sso=ok')) {
      handleSsoCallback();
      window.history.replaceState(null, '', '/');
    }
  }, [handleSsoCallback]);

  const isSetupPath = pathname === '/setup';

  // Soft-navigate once both async checks have settled — avoids hard reloads
  // (and the full-page flash they cause) that window.location.href would trigger.
  useEffect(() => {
    if (!setupChecked || loading) return;
    if (needsSetup && !isSetupPath) {
      void navigate({ to: '/setup' });
    } else if (!needsSetup && isSetupPath) {
      void navigate({ to: '/' });
    }
  }, [setupChecked, loading, needsSetup, isSetupPath, navigate]);

  if (!setupChecked || loading) {
    return (
      <div className="flex min-h-screen items-center justify-center">
        <div className="text-muted-foreground">{t('common.loading')}</div>
      </div>
    );
  }

  // Show setup page directly (no AppShell)
  if (isSetupPath && needsSetup) {
    return <SetupPage />;
  }

  // Allow the register route to render via <Outlet /> when not logged in
  // AND registration is enabled. Otherwise show the login page.
  if (!user && pathname === '/register' && registrationOpen) {
    // Wrap in ErrorBoundary so a render crash in the registration
    // form doesn't blank the entire app — without this, a malformed
    // env var or transient i18n load failure on the unauth path
    // leaves the user with no UI and no path to recovery.
    return (
      <ErrorBoundary>
        <Outlet />
      </ErrorBoundary>
    );
  }

  if (!user) {
    // Same reasoning as the register branch above: if LoginPage
    // itself crashes on render, no other UI is available — the user
    // literally cannot log in to recover. A boundary here gives them
    // at least the retry button to attempt a fresh render.
    return (
      <ErrorBoundary>
        <LoginPage onLogin={login} />
      </ErrorBoundary>
    );
  }

  return (
    <AppShell userEmail={user.email} onLogout={logout}>
      <ErrorBoundary>
        <Outlet />
      </ErrorBoundary>
      <CommandPalette />
    </AppShell>
  );
}

export function NotFoundPage() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-center justify-center py-24 text-center">
      <h1 className="text-4xl font-bold">{t('notFound.title')}</h1>
      <p className="mt-2 text-muted-foreground">{t('notFound.message')}</p>
    </div>
  );
}
