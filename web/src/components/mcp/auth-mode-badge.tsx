import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import { authModeIcon, type AuthMode } from './auth-mode-utils';

interface AuthModeBadgeProps {
  mode: AuthMode;
  /** When `compact`, render an icon-only badge with a tooltip — used in
   *  table cells where horizontal space is tight. */
  compact?: boolean;
  className?: string;
}

export function AuthModeBadge({ mode, compact = false, className }: AuthModeBadgeProps) {
  const { t } = useTranslation();
  const Icon = authModeIcon[mode];
  const title = t(`mcpServers.wizard.modes.${mode}.title`);
  const description = t(`mcpServers.wizard.modes.${mode}.description`);

  if (compact) {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={cn(
              'inline-flex h-6 w-6 items-center justify-center rounded-md border bg-background text-muted-foreground',
              className,
            )}
            aria-label={title}
          >
            <Icon className="h-3.5 w-3.5" />
          </span>
        </TooltipTrigger>
        <TooltipContent side="top">
          <div className="text-xs font-medium">{title}</div>
          <div className="text-xs text-muted-foreground max-w-[280px]">{description}</div>
        </TooltipContent>
      </Tooltip>
    );
  }

  return (
    <Badge variant="outline" className={cn('gap-1.5 font-normal', className)}>
      <Icon className="h-3.5 w-3.5" />
      {title}
    </Badge>
  );
}
