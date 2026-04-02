import {
  createRouter,
  createRootRoute,
  createRoute,
  Outlet,
} from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { useAuth } from '@/hooks/use-auth';
import { AppShell } from '@/components/layout/app-shell';
import { LoginPage } from '@/routes/login';
import { DashboardPage } from '@/routes/dashboard';
import { ProvidersPage } from '@/routes/gateway/providers';
import { ModelsPage } from '@/routes/gateway/models';
import { ApiKeysPage } from '@/routes/gateway/api-keys';
import { GatewayLogsPage } from '@/routes/gateway/logs';
import { McpServersPage } from '@/routes/mcp/servers';
import { McpToolsPage } from '@/routes/mcp/tools';
import { McpLogsPage } from '@/routes/mcp/logs';
import { UsagePage } from '@/routes/analytics/usage';
import { CostsPage } from '@/routes/analytics/costs';
import { AuditPage } from '@/routes/analytics/audit';
import { UsersPage } from '@/routes/admin/users';
import { RolesPage } from '@/routes/admin/roles';
import { SettingsPage } from '@/routes/admin/settings';
import { LogForwardersPage } from '@/routes/admin/log-forwarders';
import { ProfilePage } from '@/routes/profile';

function RootComponent() {
  const { t } = useTranslation();
  const { user, loading, login, logout } = useAuth();

  if (loading) {
    return (
      <div className="flex min-h-screen items-center justify-center">
        <div className="text-muted-foreground">{t('common.loading')}</div>
      </div>
    );
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

const gatewayLogsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/gateway/logs',
  component: GatewayLogsPage,
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

const mcpLogsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/mcp/logs',
  component: McpLogsPage,
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

const auditRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/analytics/audit',
  component: AuditPage,
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
  path: '/admin/log-forwarders',
  component: LogForwardersPage,
});

const profileRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/profile',
  component: ProfilePage,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  providersRoute,
  modelsRoute,
  apiKeysRoute,
  gatewayLogsRoute,
  mcpServersRoute,
  mcpToolsRoute,
  mcpLogsRoute,
  usageRoute,
  costsRoute,
  auditRoute,
  usersRoute,
  rolesRoute,
  settingsRoute,
  logForwardersRoute,
  profileRoute,
]);

export const router = createRouter({ routeTree });

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router;
  }
}
