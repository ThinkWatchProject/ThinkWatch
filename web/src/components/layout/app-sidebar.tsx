import { useTranslation } from 'react-i18next';
import {
  Sidebar,
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarFooter,
} from '@/components/ui/sidebar';
import {
  LayoutDashboard,
  Plug,
  BrainCircuit,
  Key,
  ScrollText,
  Server,
  Wrench,
  BarChart3,
  DollarSign,
  ClipboardList,
  Users,
  Shield,
  Settings,
  Forward,
  BookOpen,
} from 'lucide-react';
import { useNavigate, useLocation } from '@tanstack/react-router';

interface NavItem {
  titleKey: string;
  icon: typeof LayoutDashboard;
  href: string;
}

interface NavGroup {
  labelKey: string;
  items: NavItem[];
}

const navGroups: NavGroup[] = [
  {
    labelKey: 'nav.overview',
    items: [
      { titleKey: 'nav.dashboard', icon: LayoutDashboard, href: '/' },
    ],
  },
  {
    labelKey: 'nav.aiGateway',
    items: [
      { titleKey: 'nav.providers', icon: Plug, href: '/gateway/providers' },
      { titleKey: 'nav.models', icon: BrainCircuit, href: '/gateway/models' },
      { titleKey: 'nav.apiKeys', icon: Key, href: '/gateway/api-keys' },
      { titleKey: 'nav.requestLogs', icon: ScrollText, href: '/gateway/logs' },
      { titleKey: 'nav.configGuide', icon: BookOpen, href: '/gateway/guide' },
    ],
  },
  {
    labelKey: 'nav.mcpGateway',
    items: [
      { titleKey: 'nav.mcpServers', icon: Server, href: '/mcp/servers' },
      { titleKey: 'nav.tools', icon: Wrench, href: '/mcp/tools' },
      { titleKey: 'nav.mcpLogs', icon: ScrollText, href: '/mcp/logs' },
    ],
  },
  {
    labelKey: 'nav.analytics',
    items: [
      { titleKey: 'nav.usage', icon: BarChart3, href: '/analytics/usage' },
      { titleKey: 'nav.costs', icon: DollarSign, href: '/analytics/costs' },
      { titleKey: 'nav.auditLogs', icon: ClipboardList, href: '/analytics/audit' },
    ],
  },
  {
    labelKey: 'nav.admin',
    items: [
      { titleKey: 'nav.users', icon: Users, href: '/admin/users' },
      { titleKey: 'nav.roles', icon: Shield, href: '/admin/roles' },
      { titleKey: 'nav.platformLogs', icon: ClipboardList, href: '/admin/platform-logs' },
      { titleKey: 'nav.logForwarders', icon: Forward, href: '/admin/log-forwarders' },
      { titleKey: 'nav.settings', icon: Settings, href: '/admin/settings' },
    ],
  },
];

export function AppSidebar() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const currentPath = location.pathname;

  return (
    <Sidebar>
      <SidebarHeader className="p-4">
        <button
          onClick={() => navigate({ to: '/' })}
          className="flex items-center gap-2 hover:opacity-80"
        >
          <Shield className="h-6 w-6 text-primary" />
          <span className="text-lg font-semibold">AgentBastion</span>
        </button>
      </SidebarHeader>
      <SidebarContent>
        {navGroups.map((group) => (
          <SidebarGroup key={group.labelKey}>
            <SidebarGroupLabel>{t(group.labelKey)}</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {group.items.map((item) => {
                  const isActive =
                    item.href === '/'
                      ? currentPath === '/'
                      : currentPath.startsWith(item.href);
                  return (
                    <SidebarMenuItem key={item.titleKey}>
                      <SidebarMenuButton
                        isActive={isActive}
                        onClick={() => navigate({ to: item.href as '/' })}
                      >
                        <item.icon className="h-4 w-4" />
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
      <SidebarFooter className="p-4 text-xs text-muted-foreground">
        AgentBastion v0.1.0
      </SidebarFooter>
    </Sidebar>
  );
}
