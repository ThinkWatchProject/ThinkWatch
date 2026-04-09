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

const breadcrumbMap: Record<string, { sectionKey: string; pageKey: string }> = {
  '/': { sectionKey: 'nav.overview', pageKey: 'nav.dashboard' },
  '/guide': { sectionKey: 'nav.overview', pageKey: 'nav.configGuide' },
  '/gateway/providers': { sectionKey: 'nav.aiGateway', pageKey: 'nav.providers' },
  '/gateway/models': { sectionKey: 'nav.aiGateway', pageKey: 'nav.models' },
  '/gateway/api-keys': { sectionKey: 'nav.aiGateway', pageKey: 'nav.apiKeys' },
  '/mcp/servers': { sectionKey: 'nav.mcpGateway', pageKey: 'nav.mcpServers' },
  '/mcp/tools': { sectionKey: 'nav.mcpGateway', pageKey: 'nav.tools' },
  '/analytics/usage': { sectionKey: 'nav.analytics', pageKey: 'nav.usage' },
  '/analytics/costs': { sectionKey: 'nav.analytics', pageKey: 'nav.costs' },
  '/logs/gateway': { sectionKey: 'nav.logs', pageKey: 'nav.requestLogs' },
  '/logs/mcp': { sectionKey: 'nav.logs', pageKey: 'nav.mcpLogs' },
  '/logs/audit': { sectionKey: 'nav.logs', pageKey: 'nav.auditLogs' },
  '/logs/platform': { sectionKey: 'nav.logs', pageKey: 'nav.platformLogs' },
  '/logs/forwarders': { sectionKey: 'nav.logs', pageKey: 'nav.logForwarders' },
  '/admin/users': { sectionKey: 'nav.admin', pageKey: 'nav.users' },
  '/admin/teams': { sectionKey: 'nav.admin', pageKey: 'nav.teams' },
  '/admin/roles': { sectionKey: 'nav.admin', pageKey: 'nav.roles' },
  '/admin/settings': { sectionKey: 'nav.admin', pageKey: 'nav.settings' },
  '/profile': { sectionKey: 'nav.admin', pageKey: 'auth.profile' },
};

export function AppHeader() {
  const { t } = useTranslation();
  const location = useLocation();
  const crumb = breadcrumbMap[location.pathname];

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
      </div>
    </header>
  );
}
