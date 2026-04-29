import { useEffect, useState } from 'react';
import { ErrorBoundary } from '@/components/error-boundary';
import { CommandPalette } from '@/components/command-palette';
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
import { RequirePermission } from '@/components/require-permission';
import { permissionForRoute } from '@/lib/route-permissions';

/**
 * Wrap a route component with the RBAC route guard. Keeps the
 * router file declarative — `component: gate(ProvidersPage, '/gateway/providers')`
 * looks up the required permission from `route-permissions.ts` so
 * a future role rename touches one file, not twenty.
 *
 * Returning a function (not a JSX element) is what `component:`
 * expects on TanStack Router — the inner lazy component is mounted
 * inside the wrapper and Suspense still works.
 */
function gate(Component: React.ComponentType, routePath: string) {
  const perm = permissionForRoute(routePath);
  return function Gated() {
    return (
      <RequirePermission perm={perm}>
        <Component />
      </RequirePermission>
    );
  };
}

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
      <CommandPalette />
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
  component: gate(DashboardPage as unknown as React.ComponentType, '/'),
});

const providersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/providers',
  component: gate(
    ProvidersPage as unknown as React.ComponentType,
    '/gateway/providers',
  ),
});

const modelsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/models',
  component: gate(ModelsPage as unknown as React.ComponentType, '/gateway/models'),
  // `?import=<providerId>` deeplink from the Providers page — the
  // Models page reads it on mount and auto-opens the batch import
  // dialog pre-selected on that provider.
  validateSearch: (s: Record<string, unknown>) => ({
    import: typeof s.import === 'string' ? s.import : undefined,
  }),
});

const gatewaySecurityRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/security',
  component: gate(
    GatewaySecurityPage as unknown as React.ComponentType,
    '/gateway/security',
  ),
});

const apiKeysRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/api-keys',
  component: gate(ApiKeysPage as unknown as React.ComponentType, '/api-keys'),
});

const logsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/logs',
  component: gate(UnifiedLogsPage as unknown as React.ComponentType, '/logs'),
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
  component: gate(GuidePage as unknown as React.ComponentType, '/guide'),
});

const mcpServersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/servers',
  component: gate(
    McpServersPage as unknown as React.ComponentType,
    '/mcp/servers',
  ),
});

const mcpToolsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/tools',
  component: gate(McpToolsPage as unknown as React.ComponentType, '/mcp/tools'),
});

const mcpStoreRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/store',
  component: gate(McpStorePage as unknown as React.ComponentType, '/mcp/store'),
});

const usageRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/analytics/usage',
  component: gate(UsagePage as unknown as React.ComponentType, '/analytics/usage'),
});

const costsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/analytics/costs',
  component: gate(CostsPage as unknown as React.ComponentType, '/analytics/costs'),
});


const usersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/users',
  component: gate(UsersPage as unknown as React.ComponentType, '/admin/users'),
});

const teamsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/teams',
  component: gate(TeamsPage as unknown as React.ComponentType, '/admin/teams'),
});

const teamDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/teams/$id',
  component: gate(
    TeamDetailPage as unknown as React.ComponentType,
    '/admin/teams/$id',
  ),
});

const rolesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/roles',
  component: gate(RolesPage as unknown as React.ComponentType, '/admin/roles'),
});

const SETTINGS_TABS = ['general', 'auth', 'gateway', 'security', 'apikeys', 'audit', 'perf'] as const;
type SettingsTab = (typeof SETTINGS_TABS)[number];

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/settings',
  component: gate(
    SettingsPage as unknown as React.ComponentType,
    '/admin/settings',
  ),
  // Deep-link to a specific tab via `?tab=auth`. Unknown values fall
  // through as undefined so the page defaults to the first tab.
  validateSearch: (s: Record<string, unknown>): { tab?: SettingsTab } => {
    const t = s.tab;
    return {
      tab: typeof t === 'string' && (SETTINGS_TABS as readonly string[]).includes(t)
        ? (t as SettingsTab)
        : undefined,
    };
  },
});

const logForwardersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/logs/forwarders',
  component: gate(
    LogForwardersPage as unknown as React.ComponentType,
    '/logs/forwarders',
  ),
});


const apiDocsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/api-docs',
  component: gate(
    ApiDocsPage as unknown as React.ComponentType,
    '/admin/api-docs',
  ),
});

const usageLicenseRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/usage-license',
  component: gate(
    UsageLicensePage as unknown as React.ComponentType,
    '/admin/usage-license',
  ),
});

const traceIndexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/trace',
  component: gate(TracePage as unknown as React.ComponentType, '/admin/trace'),
});

const traceDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/admin/trace/$traceId',
  component: gate(
    TracePage as unknown as React.ComponentType,
    '/admin/trace/$traceId',
  ),
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
