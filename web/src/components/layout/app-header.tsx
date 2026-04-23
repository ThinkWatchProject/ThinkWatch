import { useTranslation } from 'react-i18next';
import { useLocation } from '@tanstack/react-router';
import { SidebarTrigger } from '@/components/ui/sidebar';
import { Separator } from '@/components/ui/separator';
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from '@/components/ui/breadcrumb';
import { LanguageSwitcher } from './language-switcher';
import { ThemeToggle } from './theme-toggle';
import { HeaderUserMenu } from './header-user-menu';

interface CrumbEntry {
  sectionKey: string;
  pageKey: string;
}

/// Exact path → crumb. Kept separate from the prefix list below so
/// dynamic routes (`/admin/teams/$id`, `/admin/trace/$traceId`) still
/// pick up the right section even when the concrete id is part of the
/// path.
const exactBreadcrumbs: Record<string, CrumbEntry> = {
  '/': { sectionKey: 'nav.overview', pageKey: 'nav.dashboard' },
  '/guide': { sectionKey: 'nav.overview', pageKey: 'nav.configGuide' },
  '/api-keys': { sectionKey: 'nav.overview', pageKey: 'nav.apiKeys' },
  '/gateway/providers': { sectionKey: 'nav.aiGateway', pageKey: 'nav.providers' },
  '/gateway/models': { sectionKey: 'nav.aiGateway', pageKey: 'nav.models' },
  '/gateway/security': { sectionKey: 'nav.aiGateway', pageKey: 'nav.contentSecurity' },
  '/mcp/servers': { sectionKey: 'nav.mcpGateway', pageKey: 'nav.mcpServers' },
  '/mcp/tools': { sectionKey: 'nav.mcpGateway', pageKey: 'nav.tools' },
  '/mcp/store': { sectionKey: 'nav.mcpGateway', pageKey: 'nav.mcpStore' },
  '/analytics/usage': { sectionKey: 'nav.analytics', pageKey: 'nav.usage' },
  '/analytics/costs': { sectionKey: 'nav.analytics', pageKey: 'nav.costs' },
  '/logs': { sectionKey: 'nav.logs', pageKey: 'nav.logExplorer' },
  '/logs/forwarders': { sectionKey: 'nav.logs', pageKey: 'nav.logForwarders' },
  '/admin/users': { sectionKey: 'nav.admin', pageKey: 'nav.users' },
  '/admin/teams': { sectionKey: 'nav.admin', pageKey: 'nav.teams' },
  '/admin/roles': { sectionKey: 'nav.admin', pageKey: 'nav.roles' },
  '/admin/settings': { sectionKey: 'nav.admin', pageKey: 'nav.settings' },
  '/admin/api-docs': { sectionKey: 'nav.admin', pageKey: 'nav.apiDocs' },
  '/admin/usage-license': { sectionKey: 'nav.admin', pageKey: 'nav.usageLicense' },
  '/admin/trace': { sectionKey: 'nav.logs', pageKey: 'nav.trace' },
  '/profile': { sectionKey: 'nav.admin', pageKey: 'auth.profile' },
};

/// Prefix → crumb, used for dynamic routes where the URL contains an
/// id segment. Longest prefix wins, so `/admin/teams/$id` matches
/// before `/admin/teams`.
const prefixBreadcrumbs: { prefix: string; crumb: CrumbEntry }[] = [
  { prefix: '/admin/teams/', crumb: { sectionKey: 'nav.admin', pageKey: 'nav.teams' } },
  { prefix: '/admin/trace/', crumb: { sectionKey: 'nav.logs', pageKey: 'nav.trace' } },
];

function resolveCrumb(pathname: string): CrumbEntry | undefined {
  const exact = exactBreadcrumbs[pathname];
  if (exact) return exact;
  for (const { prefix, crumb } of prefixBreadcrumbs) {
    if (pathname.startsWith(prefix)) return crumb;
  }
  return undefined;
}

interface AppHeaderProps {
  userEmail?: string;
  onLogout: () => void;
}

export function AppHeader({ userEmail, onLogout }: AppHeaderProps) {
  const { t } = useTranslation();
  const location = useLocation();
  const crumb = resolveCrumb(location.pathname);

  return (
    <header className="flex h-16 shrink-0 items-center gap-2 transition-[width,height] ease-linear group-has-data-[collapsible=icon]/sidebar-wrapper:h-12">
      <div className="flex items-center gap-2 px-4">
        <SidebarTrigger className="-ml-1" />
        <Separator
          orientation="vertical"
          className="mr-2 data-[orientation=vertical]:h-4"
        />
        <Breadcrumb>
          <BreadcrumbList>
            {crumb ? (
              <>
                <BreadcrumbItem className="hidden md:block">
                  <BreadcrumbLink href="#">
                    {t(crumb.sectionKey)}
                  </BreadcrumbLink>
                </BreadcrumbItem>
                <BreadcrumbSeparator className="hidden md:block" />
                <BreadcrumbItem>
                  <BreadcrumbPage>{t(crumb.pageKey)}</BreadcrumbPage>
                </BreadcrumbItem>
              </>
            ) : (
              <BreadcrumbItem>
                <BreadcrumbPage>ThinkWatch</BreadcrumbPage>
              </BreadcrumbItem>
            )}
          </BreadcrumbList>
        </Breadcrumb>
      </div>
      <div className="ml-auto flex items-center gap-2 px-4">
        <ThemeToggle />
        <LanguageSwitcher />
        <Separator
          orientation="vertical"
          className="mx-1 data-[orientation=vertical]:h-4"
        />
        <HeaderUserMenu userEmail={userEmail} onLogout={onLogout} />
      </div>
    </header>
  );
}
