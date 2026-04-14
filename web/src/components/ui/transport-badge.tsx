import { Zap, Radio, Terminal } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';

interface TransportBadgeProps {
  transport: string;
  className?: string;
}

const LABEL: Record<string, string> = {
  streamable_http: 'Streamable HTTP',
  sse: 'Server-Sent Events',
  stdio: 'Stdio',
};

const STYLE: Record<string, string> = {
  streamable_http: 'bg-violet-500/10 text-violet-600 border-violet-500/20 dark:text-violet-400',
  sse: 'bg-sky-500/10 text-sky-600 border-sky-500/20 dark:text-sky-400',
  stdio: 'bg-stone-500/10 text-stone-600 border-stone-500/20 dark:text-stone-400',
};

function Icon({ transport, className }: { transport: string; className?: string }) {
  if (transport === 'sse') return <Radio className={className} />;
  if (transport === 'stdio') return <Terminal className={className} />;
  return <Zap className={className} />;
}

/**
 * Compact transport-type badge. Shows an icon + shorthand with full
 * name in tooltip. Replaces the raw `streamable_http` text pill.
 */
export function TransportBadge({ transport, className }: TransportBadgeProps) {
  const style = STYLE[transport] ?? STYLE.stdio;
  const label = LABEL[transport] ?? transport;
  return (
    <TooltipProvider delayDuration={200}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={cn(
              'inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-[10px] font-medium',
              style,
              className,
            )}
          >
            <Icon transport={transport} className="h-3 w-3" />
            <span className="font-mono uppercase tracking-wide">
              {transport === 'streamable_http' ? 'HTTP' : transport === 'sse' ? 'SSE' : 'STDIO'}
            </span>
          </span>
        </TooltipTrigger>
        <TooltipContent side="top">{label}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
