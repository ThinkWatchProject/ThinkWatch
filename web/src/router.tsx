import { useEffect, useState } from 'react';
import { ErrorBoundary } from '@/components/error-boundary';
import {
  createRouter,
  createRootRoute,
  createRoute,
  lazyRouteComponent,
  Outlet,
  useNavigate,
  useRouterState,
} from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { useAuth } from '@/hooks/use-auth';
import { AppShell } from '@/components/layout/app-shell';
import { API_BASE } from '@/lib/api';
import { SetupStatusSchema, type SetupStatus } from '@/lib/schemas';
import { useSsoStatus } from '@/hooks/use-sso-status';

// Eagerly loaded — entry/auth screens that the user always hits first.
import { LoginPage } from '@/routes/login';
import { SetupPage } from '@/routes/setup';

// Lazy-loaded — split into separate chunks so the initial bundle stays small.
// Dashboard is lazy too because it pulls in recharts (~250 KB gzipped).
const DashboardPage = lazyRouteComponent(() => import('@/routes/dashboard'), 'DashboardPage');
const RegisterPage = lazyRouteComponent(() => import('@/routes/register'), 'RegisterPage');
const ProvidersPage = lazyRouteComponent(() => import('@/routes/gateway/providers'), 'ProvidersPage');
const ModelsPage = lazyRouteComponent(() => import('@/routes/gateway/models'), 'ModelsPage');
const GatewaySecurityPage = lazyRouteComponent(() => import('@/routes/gateway/security'), 'GatewaySecurityPage');
const ApiKeysPage = lazyRouteComponent(() => import('@/routes/api-keys'), 'ApiKeysPage');
const UnifiedLogsPage = lazyRouteComponent(() => import('@/routes/logs'), 'UnifiedLogsPage');
const GuidePage = lazyRouteComponent(() => import('@/routes/guide'), 'GuidePage');
const McpServersPage = lazyRouteComponent(() => import('@/routes/mcp/servers'), 'McpServersPage');
const McpToolsPage = lazyRouteComponent(() => import('@/routes/mcp/tools'), 'McpToolsPage');
const McpStorePage = lazyRouteComponent(() => import('@/routes/mcp/store'), 'McpStorePage');
const UsagePage = lazyRouteComponent(() => import('@/routes/analytics/usage'), 'UsagePage');
const CostsPage = lazyRouteComponent(() => import('@/routes/analytics/costs'), 'CostsPage');
const UsersPage = lazyRouteComponent(() => import('@/routes/admin/users'), 'UsersPage');
const TeamsPage = lazyRouteComponent(() => import('@/routes/admin/teams'), 'TeamsPage');
const TeamDetailPage = lazyRouteComponent(() => import('@/routes/admin/team-detail'), 'TeamDetailPage');
const RolesPage = lazyRouteComponent(() => import('@/routes/admin/roles'), 'RolesPage');
const SettingsPage = lazyRouteComponent(() => import('@/routes/admin/settings'), 'SettingsPage');
const LogForwardersPage = lazyRouteComponent(() => import('@/routes/admin/log-forwarders'), 'LogForwardersPage');
const ApiDocsPage = lazyRouteComponent(() => import('@/routes/admin/api-docs'), 'ApiDocsPage');
const UsageLicensePage = lazyRouteComponent(
  () => import('@/routes/admin/usage-license'),
  'UsageLicensePage',
);
const TracePage = lazyRouteComponent(() => import('@/routes/admin/trace'), 'TracePage');
const WebhookOutboxPage = lazyRouteComponent(
  () => import('@/routes/admin/webhook-outbox'),
  'WebhookOutboxPage',
);
const ProfilePage = lazyRouteComponent(() => import('@/routes/profile'), 'ProfilePage');

let cachedSetupStatus: SetupStatus | null = null;

/// Force the next mount to re-fetch /api/setup/status. Called by the
/// setup wizard after a successful initialize so the user lands on the
/// real app immediately, without a hard refresh.
export function invalidateSetupStatusCache() {
  cachedSetupStatus = null;
}

function RootComponent() {
  const { t } = useTranslation();
  const { user, loading, login, logout, handleSsoCallback } = useAuth();
  const [setupChecked, setSetupChecked] = useState(cachedSetupStatus !== null);
  const [needsSetup, setNeedsSetup] = useState(cachedSetupStatus?.needs_setup ?? false);
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
          cachedSetupStatus = data;
          setNeedsSetup(data.needs_setup);
          setSetupChecked(true);
        })
        .catch(() => {
          if (cancelled) return;
          cachedSetupStatus = { initialized: true, needs_setup: false };
          setSetupChecked(true);
        });
    };
    if (cachedSetupStatus === null) check();
    const onVis = () => {
      // When the tab becomes visible, re-check IF the cache was invalidated
      // (or if we're still in needs_setup state — covers the case where the
      // user just finished setup in this tab).
      if (!document.hidden && (cachedSetupStatus === null || cachedSetupStatus.needs_setup)) {
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
    return <Outlet />;
  }

  if (!user) {
    return <LoginPage onLogin={login} />;
  }

  return (
    <AppShell userEmail={user.email} onLogout={logout}>
      <ErrorBoundary>
        <Outlet />
      </ErrorBoundary>
    </AppShell>
  );
}

function NotFoundPage() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-center justify-center py-24 text-center">
      <h1 className="text-4xl font-bold">{t('notFound.title')}</h1>
      <p className="mt-2 text-muted-foreground">{t('notFound.message')}</p>
    </div>
  );
}

const rootRoute = createRootRoute({
  component: RootComponent,
  notFoundComponent: NotFoundPage,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: DashboardPage,
});

const providersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/providers',
  component: ProvidersPage,
});

const modelsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/models',
  component: ModelsPage,
});

const gatewaySecurityRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/security',
  component: GatewaySecurityPage,
});

const apiKeysRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/api-keys',
  component: ApiKeysPage,
});

const logsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/logs',
  component: UnifiedLogsPage,
  validateSearch: (search: Record<string, unknown>) => ({
    category: typeof search.category === 'string' ? search.category : undefined,
    q: typeof search.q === 'string' ? search.q : undefined,
    from: typeof search.from === 'string' ? search.from : undefined,
    to: typeof search.to === 'string' ? search.to : undefined,
    page: typeof search.page === 'number' ? search.page : undefined,
  }),
});

const guideRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/guide',
  component: GuidePage,
});

const mcpServersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/servers',
  component: McpServersPage,
});

const mcpToolsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/tools',
  component: McpToolsPage,
});

const mcpStoreRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/store',
  component: McpStorePage,
});

const usageRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/analytics/usage',
  component: UsagePage,
});

const costsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/analytics/costs',
  component: CostsPage,
});


const usersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/users',
  component: UsersPage,
});

const teamsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/teams',
  component: TeamsPage,
});

const teamDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/teams/$id',
  component: TeamDetailPage,
});

const rolesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/roles',
  component: RolesPage,
});

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/settings',
  component: SettingsPage,
});

const logForwardersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/logs/forwarders',
  component: LogForwardersPage,
});


const apiDocsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/api-docs',
  component: ApiDocsPage,
});

const usageLicenseRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/usage-license',
  component: UsageLicensePage,
});

const traceIndexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/trace',
  component: TracePage,
});

const traceDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/trace/$traceId',
  component: TracePage,
});

const webhookOutboxRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/webhook-outbox',
  component: WebhookOutboxPage,
});

const profileRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/profile',
  component: ProfilePage,
});

const registerRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/register',
  component: () => {
    return (
      <RegisterPage
        onRegistered={() => {
          // Hard navigate so RootComponent remounts and picks up
          // the freshly-set auth cookies via useAuth → fetchUser.
          window.location.href = '/';
        }}
      />
    );
  },
});

const setupRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/setup',
  component: SetupPage,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  providersRoute,
  modelsRoute,
  gatewaySecurityRoute,
  apiKeysRoute,
  logsRoute,
  guideRoute,
  mcpServersRoute,
  mcpToolsRoute,
  mcpStoreRoute,
  usageRoute,
  costsRoute,
  usersRoute,
  teamsRoute,
  teamDetailRoute,
  rolesRoute,
  settingsRoute,
  logForwardersRoute,
  apiDocsRoute,
  usageLicenseRoute,
  traceIndexRoute,
  traceDetailRoute,
  webhookOutboxRoute,
  profileRoute,
  registerRoute,
  setupRoute,
]);

export const router = createRouter({ routeTree });

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router;
  }
}
