import { useTranslation } from 'react-i18next';
import { SidebarTrigger } from '@/components/ui/sidebar';
import { Separator } from '@/components/ui/separator';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { LogOut, User } from 'lucide-react';
import { useNavigate } from '@tanstack/react-router';
import { LanguageSwitcher } from './language-switcher';
import { ThemeToggle } from './theme-toggle';

interface AppHeaderProps {
  userEmail?: string;
  onLogout: () => void;
}

export function AppHeader({ userEmail, onLogout }: AppHeaderProps) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const initials = userEmail
    ? userEmail.substring(0, 2).toUpperCase()
    : 'AB';

  return (
    <header className="flex h-14 items-center gap-2 border-b px-4">
      <SidebarTrigger />
      <Separator orientation="vertical" className="h-6" />
      <div className="flex-1" />
      <ThemeToggle />
      <LanguageSwitcher />
      <DropdownMenu>
        <DropdownMenuTrigger
          render={<button className="flex items-center gap-2 rounded-md p-1 hover:bg-accent" />}
        >
          <Avatar className="h-8 w-8">
            <AvatarFallback className="text-xs">{initials}</AvatarFallback>
          </Avatar>
          <span className="hidden text-sm md:inline">{userEmail}</span>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onClick={() => navigate({ to: '/profile' })}>
            <User className="mr-2 h-4 w-4" />
            {t('auth.profile')}
          </DropdownMenuItem>
          <DropdownMenuItem onClick={onLogout}>
            <LogOut className="mr-2 h-4 w-4" />
            {t('auth.logout')}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </header>
  );
}
