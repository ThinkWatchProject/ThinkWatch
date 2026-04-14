import { Lock, Globe, KeyRound } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';

interface AuthBadgeProps {
  /** Auth type string from the backend (`bearer`, `api_key`, `none`, or null). */
  authType: string | null | undefined;
  /** Label for "requires auth" / "no auth" tooltip context. */
  requiredLabel: string;
  noneLabel: string;
  className?: string;
}

/**
 * Icon-only auth-required indicator. Uses the shadcn `Badge` at a square
 * footprint so it stays visually consistent with other chips in the row
 * while taking minimal table width.
 */
export function AuthBadge({ authType, requiredLabel, noneLabel, className }: AuthBadgeProps) {
  const required = !!authType && authType !== 'none';
  const Icon = required ? (authType === 'api_key' ? KeyRound : Lock) : Globe;
  const tooltip = required
    ? `${requiredLabel} (${authType === 'api_key' ? 'API Key' : 'Bearer'})`
    : noneLabel;
  const tint = required
    ? 'bg-amber-500/10 text-amber-600 dark:text-amber-400'
    : 'bg-muted/40 text-muted-foreground';

  return (
    <TooltipProvider delayDuration={200}>
      <Tooltip>
        <TooltipTrigger asChild>
          <Badge variant="outline" className={cn('h-5 w-5 p-0', tint, className)}>
            <Icon />
          </Badge>
        </TooltipTrigger>
        <TooltipContent side="top">{tooltip}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
