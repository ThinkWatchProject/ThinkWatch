import { Zap, Radio, Terminal } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';

interface TransportBadgeProps {
  transport: string;
  className?: string;
}

const LABEL: Record<string, string> = {
  streamable_http: 'Streamable HTTP',
  sse: 'Server-Sent Events',
  stdio: 'Stdio',
};

const TINT: Record<string, string> = {
  streamable_http: 'bg-violet-500/10 text-violet-600 dark:text-violet-400',
  sse: 'bg-sky-500/10 text-sky-600 dark:text-sky-400',
  stdio: 'bg-stone-500/10 text-stone-600 dark:text-stone-400',
};

function Icon({ transport, className }: { transport: string; className?: string }) {
  if (transport === 'sse') return <Radio className={className} />;
  if (transport === 'stdio') return <Terminal className={className} />;
  return <Zap className={className} />;
}

/**
 * Compact transport-type chip. Composes the shadcn `Badge` primitive with
 * a tinted className + lucide icon, so it participates in the Badge design
 * tokens (focus ring, slot data attrs) while carrying a per-transport
 * color accent that the stock variants don't offer.
 */
export function TransportBadge({ transport, className }: TransportBadgeProps) {
  const tint = TINT[transport] ?? TINT.stdio;
  const label = LABEL[transport] ?? transport;
  const short = transport === 'streamable_http' ? 'HTTP' : transport === 'sse' ? 'SSE' : 'STDIO';
  return (
    <TooltipProvider delayDuration={200}>
      <Tooltip>
        <TooltipTrigger asChild>
          <Badge variant="secondary" className={cn('font-mono uppercase', tint, className)}>
            <Icon transport={transport} />
            {short}
          </Badge>
        </TooltipTrigger>
        <TooltipContent side="top">{label}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
