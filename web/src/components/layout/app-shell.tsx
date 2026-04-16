import type { ReactNode } from 'react';
import { SidebarProvider, SidebarInset } from '@/components/ui/sidebar';
import { TooltipProvider } from '@/components/ui/tooltip';
import { AppSidebar } from './app-sidebar';
import { AppHeader } from './app-header';

interface AppShellProps {
  children: ReactNode;
  userEmail?: string;
  onLogout: () => void;
}

export function AppShell({ children, userEmail, onLogout }: AppShellProps) {
  return (
    <TooltipProvider>
      <SidebarProvider>
        <AppSidebar userEmail={userEmail} onLogout={onLogout} />
        <SidebarInset>
          <AppHeader />
          <div className="flex flex-1 flex-col gap-4 p-4 pt-0 overflow-hidden">
            {children}
          </div>
        </SidebarInset>
      </SidebarProvider>
    </TooltipProvider>
  );
}
