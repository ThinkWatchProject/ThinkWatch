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
  useSidebar,
} from '@/components/ui/sidebar';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
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
  Users,
  UsersRound,
  Shield,
  Settings,
  Forward,
  BookOpen,
  Menu,
  LogOut,
  User,
} from 'lucide-react';
import { useNavigate, useLocation } from '@tanstack/react-router';
import { ThinkWatchMark } from '@/components/brand/think-watch-mark';

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
      { titleKey: 'nav.configGuide', icon: BookOpen, href: '/guide' },
    ],
  },
  {
    labelKey: 'nav.aiGateway',
    items: [
      { titleKey: 'nav.providers', icon: Plug, href: '/gateway/providers' },
      { titleKey: 'nav.models', icon: BrainCircuit, href: '/gateway/models' },
      { titleKey: 'nav.apiKeys', icon: Key, href: '/gateway/api-keys' },
    ],
  },
  {
    labelKey: 'nav.mcpGateway',
    items: [
      { titleKey: 'nav.mcpServers', icon: Server, href: '/mcp/servers' },
      { titleKey: 'nav.tools', icon: Wrench, href: '/mcp/tools' },
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
      {
        titleKey: 'nav.logForwarders',
        icon: Forward,
        href: '/logs/forwarders',
      },
    ],
  },
  {
    labelKey: 'nav.admin',
    items: [
      { titleKey: 'nav.users', icon: Users, href: '/admin/users' },
      { titleKey: 'nav.teams', icon: UsersRound, href: '/admin/teams' },
      { titleKey: 'nav.roles', icon: Shield, href: '/admin/roles' },
      { titleKey: 'nav.settings', icon: Settings, href: '/admin/settings' },
    ],
  },
];

function NavUser({
  userEmail,
  onLogout,
}: {
  userEmail?: string;
  onLogout: () => void;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { isMobile } = useSidebar();
  const initials = userEmail
    ? userEmail.substring(0, 2).toUpperCase()
    : 'AB';

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton
              size="lg"
              className="data-[state=open]:bg-sidebar-accent data-[state=open]:text-sidebar-accent-foreground"
            >
              <Avatar className="h-8 w-8 rounded-lg">
                <AvatarFallback className="rounded-lg text-xs">
                  {initials}
                </AvatarFallback>
              </Avatar>
              <div className="grid flex-1 text-left text-sm leading-tight">
                <span className="truncate font-medium">
                  {userEmail ?? 'User'}
                </span>
                <span className="truncate text-xs text-muted-foreground">
                  {userEmail}
                </span>
              </div>
              <Menu className="ml-auto size-4" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            className="w-(--radix-dropdown-menu-trigger-width) min-w-56 rounded-lg"
            side={isMobile ? 'bottom' : 'right'}
            align="end"
            sideOffset={4}
          >
            <DropdownMenuLabel className="p-0 font-normal">
              <div className="flex items-center gap-2 px-1 py-1.5 text-left text-sm">
                <Avatar className="h-8 w-8 rounded-lg">
                  <AvatarFallback className="rounded-lg text-xs">
                    {initials}
                  </AvatarFallback>
                </Avatar>
                <div className="grid flex-1 text-left text-sm leading-tight">
                  <span className="truncate font-medium">
                    {userEmail ?? 'User'}
                  </span>
                  <span className="truncate text-xs">{userEmail}</span>
                </div>
              </div>
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={() => navigate({ to: '/profile' })}>
              <User />
              {t('auth.profile')}
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={onLogout}>
              <LogOut />
              {t('auth.logout')}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}

export function AppSidebar({
  userEmail,
  onLogout,
}: {
  userEmail?: string;
  onLogout: () => void;
}) {
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
        {navGroups.map((group) => (
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
                        onClick={() =>
                          navigate({ to: item.href as '/' })
                        }
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
        <NavUser userEmail={userEmail} onLogout={onLogout} />
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
