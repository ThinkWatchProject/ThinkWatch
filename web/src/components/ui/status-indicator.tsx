import { cn } from '@/lib/utils';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';

export type StatusKind = 'healthy' | 'degraded' | 'down' | 'unknown' | 'inactive';

interface StatusIndicatorProps {
  status: StatusKind;
  /** Accessible label / tooltip text. */
  label: string;
  /** Show the label inline instead of just a dot. */
  showLabel?: boolean;
  /** Apply a subtle breathing pulse to the dot. */
  pulse?: boolean;
  className?: string;
}

const dotColor: Record<StatusKind, string> = {
  healthy: 'bg-emerald-500',
  degraded: 'bg-amber-500',
  down: 'bg-rose-500',
  unknown: 'bg-muted-foreground/50',
  inactive: 'bg-muted-foreground/30',
};

const dotGlow: Record<StatusKind, string> = {
  healthy: 'shadow-[0_0_6px_theme(colors.emerald.500/0.7)]',
  degraded: 'shadow-[0_0_6px_theme(colors.amber.500/0.7)]',
  down: 'shadow-[0_0_6px_theme(colors.rose.500/0.7)]',
  unknown: '',
  inactive: '',
};

const labelColor: Record<StatusKind, string> = {
  healthy: 'text-emerald-600 dark:text-emerald-400',
  degraded: 'text-amber-600 dark:text-amber-400',
  down: 'text-rose-600 dark:text-rose-400',
  unknown: 'text-muted-foreground',
  inactive: 'text-muted-foreground',
};

/**
 * Compact status indicator: a colored dot with optional label and
 * breathing pulse. The dot glows subtly to read as "alive" instead of
 * a flat decoration. Wrapped in a tooltip so colorblind/screen-reader
 * users get the status as text.
 */
export function StatusIndicator({
  status,
  label,
  showLabel = false,
  pulse = false,
  className,
}: StatusIndicatorProps) {
  const dot = (
    <span className="relative inline-flex">
      <span
        className={cn(
          'inline-block h-2 w-2 rounded-full',
          dotColor[status],
          dotGlow[status],
        )}
      />
      {pulse && (status === 'healthy' || status === 'down') && (
        <span
          className={cn(
            'absolute inset-0 inline-flex h-2 w-2 animate-ping rounded-full opacity-60',
            dotColor[status],
          )}
        />
      )}
    </span>
  );

  if (showLabel) {
    return (
      <span className={cn('inline-flex items-center gap-1.5', className)}>
        {dot}
        <span className={cn('text-xs font-medium', labelColor[status])}>{label}</span>
      </span>
    );
  }

  return (
    <TooltipProvider delayDuration={200}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            role="img"
            aria-label={label}
            className={cn('inline-flex cursor-help items-center', className)}
          >
            {dot}
          </span>
        </TooltipTrigger>
        <TooltipContent side="top">{label}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
