import { useTranslation } from 'react-i18next';
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
} from '@/components/ui/sidebar';
import {
  LayoutDashboard,
  Plug,
  Plug2,
  BrainCircuit,
  Key,
  ScrollText,
  Server,
  Wrench,
  BarChart3,
  DollarSign,
  Users,
  UsersRound,
  Shield,
  Settings,
  Forward,
  BookOpen,
  FileCode2,
  Gauge,
  GitBranch,
  Filter,
  Store,
} from 'lucide-react';
import { useNavigate, useLocation } from '@tanstack/react-router';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';
import { hasPermission } from '@/lib/api';
import { permissionForRoute } from '@/lib/route-permissions';
import { SidebarSystemStatus } from './sidebar-system-status';

interface NavItem {
  titleKey: string;
  icon: typeof LayoutDashboard;
  href: string;
}
interface NavGroup {
  labelKey: string;
  items: NavItem[];
}

// Per-item permission lives in `route-permissions.ts` (single source
// of truth shared with the router guard). This file owns layout
// concerns only — labels, icons, group order. To gate a new entry,
// add the route's permission to `route-permissions.ts` and the
// sidebar / route guard pick it up automatically.
const navGroups: NavGroup[] = [
  {
    labelKey: 'nav.overview',
    items: [
      { titleKey: 'nav.dashboard', icon: LayoutDashboard, href: '/' },
      { titleKey: 'nav.apiKeys', icon: Key, href: '/api-keys' },
      { titleKey: 'nav.configGuide', icon: BookOpen, href: '/guide' },
    ],
  },
  {
    labelKey: 'nav.aiGateway',
    items: [
      { titleKey: 'nav.providers', icon: Plug, href: '/gateway/providers' },
      { titleKey: 'nav.models', icon: BrainCircuit, href: '/gateway/models' },
      { titleKey: 'nav.contentSecurity', icon: Filter, href: '/gateway/security' },
    ],
  },
  {
    labelKey: 'nav.mcpGateway',
    items: [
      { titleKey: 'nav.mcpServers', icon: Server, href: '/mcp/servers' },
      { titleKey: 'nav.tools', icon: Wrench, href: '/mcp/tools' },
      { titleKey: 'nav.mcpStore', icon: Store, href: '/mcp/store' },
      { titleKey: 'nav.connections', icon: Plug2, href: '/connections' },
    ],
  },
  {
    labelKey: 'nav.analytics',
    items: [
      { titleKey: 'nav.usage', icon: BarChart3, href: '/analytics/usage' },
      { titleKey: 'nav.costs', icon: DollarSign, href: '/analytics/costs' },
    ],
  },
  {
    labelKey: 'nav.logs',
    items: [
      { titleKey: 'nav.logExplorer', icon: ScrollText, href: '/logs' },
      { titleKey: 'nav.trace', icon: GitBranch, href: '/admin/trace' },
      { titleKey: 'nav.logForwarders', icon: Forward, href: '/logs/forwarders' },
    ],
  },
  {
    labelKey: 'nav.admin',
    items: [
      { titleKey: 'nav.users', icon: Users, href: '/admin/users' },
      { titleKey: 'nav.teams', icon: UsersRound, href: '/admin/teams' },
      { titleKey: 'nav.roles', icon: Shield, href: '/admin/roles' },
      { titleKey: 'nav.settings', icon: Settings, href: '/admin/settings' },
      { titleKey: 'nav.apiDocs', icon: FileCode2, href: '/admin/api-docs' },
      { titleKey: 'nav.usageLicense', icon: Gauge, href: '/admin/usage-license' },
    ],
  },
];

export function AppSidebar() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const currentPath = location.pathname;

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton
              size="lg"
              onClick={() => navigate({ to: '/' })}
              className="h-14 py-2"
            >
              <div className="flex aspect-square size-10 items-center justify-center rounded-xl bg-primary text-primary-foreground">
                {/* SidebarMenuButton has a descendant rule [&_svg]:size-4 that
                    would shrink the brand mark to 16px. Override with !size-7. */}
                <ThinkWatchMark className="!size-7" />
              </div>
              <div className="grid flex-1 text-left leading-tight">
                <span className="truncate text-base font-bold tracking-tight">ThinkWatch</span>
                <span className="truncate text-xs text-muted-foreground">
                  Enterprise
                </span>
              </div>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        {/*
         * Permission-aware nav rendering. Each item declares the
         * permission its target route requires; we drop the item
         * (and any group that becomes empty) when the user lacks
         * the perm so a developer-only login doesn't surface admin
         * sections they'll just see a 403 on. Defines what UX-10
         * meant by "hide instead of grey out". Page-level action
         * buttons inside surviving routes have their own
         * hasPermission() gates — the route reaches a permitted
         * user, the action only renders if they can act.
         */}
        {navGroups
          .map((group) => ({
            ...group,
            items: group.items.filter((item) => {
              const perm = permissionForRoute(item.href);
              return !perm || hasPermission(perm);
            }),
          }))
          .filter((group) => group.items.length > 0)
          .map((group) => (
          <SidebarGroup key={group.labelKey}>
            <SidebarGroupLabel>{t(group.labelKey)}</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {group.items.map((item) => {
                  // Exact match only. Sub-paths are not auto-highlighted to
                  // avoid sibling collisions like /logs vs /logs/forwarders.
                  const isActive = currentPath === item.href;
                  return (
                    <SidebarMenuItem key={item.titleKey}>
                      <SidebarMenuButton
                        tooltip={t(item.titleKey)}
                        isActive={isActive}
                        onClick={() => navigate({ to: item.href as '/' })}
                      >
                        <item.icon />
                        <span>{t(item.titleKey)}</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  );
                })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ))}
      </SidebarContent>
      <SidebarFooter>
        <SidebarSystemStatus />
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
