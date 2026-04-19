import { useTranslation } from 'react-i18next';
import { useSidebar } from '@/components/ui/sidebar';
import { StatusIndicator, type StatusKind } from '@/components/ui/status-indicator';
import { useSystemHealth, type SystemStatus } from '@/hooks/use-system-health';
import { cn } from '@/lib/utils';

const kindMap: Record<SystemStatus, StatusKind> = {
  operational: 'healthy',
  degraded: 'degraded',
  down: 'down',
  unknown: 'unknown',
};

export function SidebarSystemStatus() {
  const { t } = useTranslation();
  const { state } = useSidebar();
  const status = useSystemHealth();
  const kind = kindMap[status];
  const label = t(`systemStatus.${status}`);

  if (state === 'collapsed') {
    return (
      <div className="flex justify-center py-2">
        <StatusIndicator status={kind} label={label} pulse={status === 'operational'} />
      </div>
    );
  }

  return (
    <div className="border-t border-sidebar-border px-2 pt-2 pb-1">
      <div className="flex items-center gap-2 px-2 text-xs">
        <StatusIndicator
          status={kind}
          label={label}
          showLabel
          pulse={status === 'operational'}
        />
      </div>
      <div
        className={cn(
          'px-2 pt-0.5 font-mono text-[10px] tracking-wide text-muted-foreground',
        )}
      >
        v{__APP_VERSION__}
      </div>
    </div>
  );
}
