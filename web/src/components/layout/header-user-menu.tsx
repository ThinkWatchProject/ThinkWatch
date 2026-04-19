import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
import { LogOut, User } from 'lucide-react';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';

interface HeaderUserMenuProps {
  userEmail?: string;
  onLogout: () => void;
}

export function HeaderUserMenu({ userEmail, onLogout }: HeaderUserMenuProps) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const initials = userEmail ? userEmail.substring(0, 2).toUpperCase() : 'AB';

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          className="h-8 gap-2 px-1.5 data-[state=open]:bg-accent"
        >
          <Avatar className="h-6 w-6">
            <AvatarFallback className="text-[10px]">{initials}</AvatarFallback>
          </Avatar>
          <span className="hidden max-w-[160px] truncate text-xs font-medium md:inline">
            {userEmail ?? 'User'}
          </span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuLabel className="p-0 font-normal">
          <div className="flex items-center gap-2 px-2 py-1.5 text-left text-sm">
            <Avatar className="h-8 w-8 rounded-lg">
              <AvatarFallback className="rounded-lg text-xs">{initials}</AvatarFallback>
            </Avatar>
            <div className="grid flex-1 text-left text-sm leading-tight">
              <span className="truncate font-medium">{userEmail ?? 'User'}</span>
              <span className="truncate text-xs text-muted-foreground">
                {userEmail}
              </span>
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
  );
}
