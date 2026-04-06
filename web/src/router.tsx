import { useEffect, useState } from 'react';
import {
  createRouter,
  createRootRoute,
  createRoute,
  lazyRouteComponent,
  Outlet,
} from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { useAuth } from '@/hooks/use-auth';
import { AppShell } from '@/components/layout/app-shell';

// Eagerly loaded — entry/auth screens that the user always hits first.
import { LoginPage } from '@/routes/login';
import { SetupPage } from '@/routes/setup';
import { DashboardPage } from '@/routes/dashboard';

// Lazy-loaded — split into separate chunks so the initial bundle stays small.
const RegisterPage = lazyRouteComponent(() => import('@/routes/register'), 'RegisterPage');
const ProvidersPage = lazyRouteComponent(() => import('@/routes/gateway/providers'), 'ProvidersPage');
const ModelsPage = lazyRouteComponent(() => import('@/routes/gateway/models'), 'ModelsPage');
const ApiKeysPage = lazyRouteComponent(() => import('@/routes/gateway/api-keys'), 'ApiKeysPage');
const UnifiedLogsPage = lazyRouteComponent(() => import('@/routes/logs'), 'UnifiedLogsPage');
const GuidePage = lazyRouteComponent(() => import('@/routes/guide'), 'GuidePage');
const McpServersPage = lazyRouteComponent(() => import('@/routes/mcp/servers'), 'McpServersPage');
const McpToolsPage = lazyRouteComponent(() => import('@/routes/mcp/tools'), 'McpToolsPage');
const UsagePage = lazyRouteComponent(() => import('@/routes/analytics/usage'), 'UsagePage');
const CostsPage = lazyRouteComponent(() => import('@/routes/analytics/costs'), 'CostsPage');
const UsersPage = lazyRouteComponent(() => import('@/routes/admin/users'), 'UsersPage');
const RolesPage = lazyRouteComponent(() => import('@/routes/admin/roles'), 'RolesPage');
const SettingsPage = lazyRouteComponent(() => import('@/routes/admin/settings'), 'SettingsPage');
const LogForwardersPage = lazyRouteComponent(() => import('@/routes/admin/log-forwarders'), 'LogForwardersPage');
const ProfilePage = lazyRouteComponent(() => import('@/routes/profile'), 'ProfilePage');

const API_BASE = import.meta.env.VITE_API_BASE ?? '';

let cachedSetupStatus: { initialized: boolean; needs_setup: boolean } | null = null;

function RootComponent() {
  const { t } = useTranslation();
  const { user, loading, login, logout, handleSsoCallback } = useAuth();
  const [setupChecked, setSetupChecked] = useState(cachedSetupStatus !== null);
  const [needsSetup, setNeedsSetup] = useState(cachedSetupStatus?.needs_setup ?? false);

  // Check setup status on mount
  useEffect(() => {
    if (cachedSetupStatus !== null) return;
    fetch(`${API_BASE}/api/setup/status`)
      .then((r) => r.json())
      .then((data: { initialized: boolean; needs_setup: boolean }) => {
        cachedSetupStatus = data;
        setNeedsSetup(data.needs_setup);
        setSetupChecked(true);
      })
      .catch(() => {
        // If the endpoint fails, assume setup is done
        cachedSetupStatus = { initialized: true, needs_setup: false };
        setSetupChecked(true);
      });
  }, []);

  // Handle SSO callback: tokens in URL fragment
  useEffect(() => {
    const hash = window.location.hash;
    if (hash.includes('access_token=')) {
      const params = new URLSearchParams(hash.slice(1));
      const accessToken = params.get('access_token');
      const refreshToken = params.get('refresh_token');
      const signingKey = params.get('signing_key');
      if (accessToken && refreshToken && signingKey) {
        handleSsoCallback(accessToken, refreshToken, signingKey);
        window.history.replaceState(null, '', '/');
      }
    }
  }, [handleSsoCallback]);

  if (!setupChecked || loading) {
    return (
      <div className="flex min-h-screen items-center justify-center">
        <div className="text-muted-foreground">{t('common.loading')}</div>
      </div>
    );
  }

  const isSetupPath = window.location.pathname === '/setup';

  // Redirect to /setup if needs setup and not already there
  if (needsSetup && !isSetupPath) {
    window.location.href = '/setup';
    return null;
  }

  // Redirect away from /setup if already initialized
  if (!needsSetup && isSetupPath) {
    window.location.href = '/';
    return null;
  }

  // Show setup page directly (no AppShell)
  if (isSetupPath && needsSetup) {
    return <SetupPage />;
  }

  if (!user) {
    return <LoginPage onLogin={login} />;
  }

  return (
    <AppShell userEmail={user.email} onLogout={logout}>
      <Outlet />
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

const apiKeysRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/api-keys',
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


const profileRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/profile',
  component: ProfilePage,
});

const registerRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/register',
  component: () => {
    const navigate = registerRoute.useNavigate();
    return <RegisterPage onRegistered={() => navigate({ to: '/' })} />;
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
  apiKeysRoute,
  logsRoute,
  guideRoute,
  mcpServersRoute,
  mcpToolsRoute,
  usageRoute,
  costsRoute,
  usersRoute,
  rolesRoute,
  settingsRoute,
  logForwardersRoute,
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
