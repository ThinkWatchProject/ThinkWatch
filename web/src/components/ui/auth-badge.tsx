import { Lock, Globe, KeyRound } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';

interface AuthBadgeProps {
  /** Auth type string from the backend (`bearer`, `api_key`, `none`, or null). */
  authType: string | null | undefined;
  /** Label for "requires auth" / "no auth" tooltip context. */
  requiredLabel: string;
  noneLabel: string;
  className?: string;
}

/**
 * Compact auth-required indicator. Icon-only with tooltip. Replaces the
 * "Auth Required" / "No Auth" text pills that were dominating row width.
 */
export function AuthBadge({ authType, requiredLabel, noneLabel, className }: AuthBadgeProps) {
  const required = !!authType && authType !== 'none';
  const Icon = required ? (authType === 'api_key' ? KeyRound : Lock) : Globe;
  const tooltip = required
    ? `${requiredLabel} (${authType === 'api_key' ? 'API Key' : 'Bearer'})`
    : noneLabel;
  const style = required
    ? 'border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400'
    : 'border-border/60 bg-muted/40 text-muted-foreground';

  return (
    <TooltipProvider delayDuration={200}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={cn(
              'inline-flex h-5 w-5 items-center justify-center rounded-md border',
              style,
              className,
            )}
          >
            <Icon className="h-3 w-3" />
          </span>
        </TooltipTrigger>
        <TooltipContent side="top">{tooltip}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
