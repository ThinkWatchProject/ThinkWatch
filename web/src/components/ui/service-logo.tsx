import { Server } from 'lucide-react';
import { cn } from '@/lib/utils';

interface ServiceLogoProps {
  /** Service name — matches loosely, case-insensitive. */
  service: string;
  className?: string;
}

/**
 * Small visual "logo chip" for a named service. Not true brand logos
 * (avoids trademark risk) — uses a lettermark with brand-adjacent colors.
 * Unknown services fall back to a generic server icon.
 */
export function ServiceLogo({ service, className }: ServiceLogoProps) {
  const key = service.toLowerCase();
  const spec = resolve(key);

  if (!spec) {
    return (
      <span
        className={cn(
          'inline-flex h-6 w-6 items-center justify-center rounded border border-border bg-muted text-muted-foreground',
          className,
        )}
      >
        <Server className="h-3.5 w-3.5" />
      </span>
    );
  }

  return (
    <span
      className={cn(
        'inline-flex h-6 w-6 items-center justify-center rounded font-mono text-[10px] font-bold',
        spec.className,
        className,
      )}
      title={service}
    >
      {spec.letter}
    </span>
  );
}

function resolve(key: string): { letter: string; className: string } | null {
  // Provider / AI services
  if (key.includes('openai') || key === 'gpt') return { letter: 'O', className: 'bg-emerald-500/15 text-emerald-500' };
  if (key.includes('anthropic') || key.includes('claude')) return { letter: 'A', className: 'bg-amber-500/15 text-amber-500' };
  if (key.includes('google') || key.includes('gemini')) return { letter: 'G', className: 'bg-blue-500/15 text-blue-500' };
  if (key.includes('azure')) return { letter: 'Az', className: 'bg-cyan-500/15 text-cyan-500' };
  if (key.includes('bedrock') || key.includes('aws')) return { letter: 'A', className: 'bg-violet-500/15 text-violet-500' };

  // Dev tooling
  if (key.includes('github')) return { letter: 'GH', className: 'bg-neutral-500/15 text-foreground' };
  if (key.includes('gitlab')) return { letter: 'GL', className: 'bg-orange-500/15 text-orange-500' };
  if (key.includes('linear')) return { letter: 'L', className: 'bg-indigo-500/15 text-indigo-500' };
  if (key.includes('sentry')) return { letter: 'S', className: 'bg-purple-500/15 text-purple-500' };
  if (key.includes('jira') || key.includes('atlassian')) return { letter: 'J', className: 'bg-blue-600/15 text-blue-600' };

  // Data stores
  if (key.includes('postgres')) return { letter: 'Pg', className: 'bg-sky-500/15 text-sky-500' };
  if (key.includes('mysql')) return { letter: 'My', className: 'bg-orange-600/15 text-orange-600' };
  if (key.includes('redis')) return { letter: 'R', className: 'bg-red-500/15 text-red-500' };
  if (key.includes('mongo')) return { letter: 'M', className: 'bg-green-500/15 text-green-500' };

  // Messaging
  if (key.includes('slack')) return { letter: 'Sl', className: 'bg-fuchsia-500/15 text-fuchsia-500' };
  if (key.includes('discord')) return { letter: 'D', className: 'bg-indigo-500/15 text-indigo-500' };

  // Docs / Knowledge
  if (key.includes('microsoft') || key.includes('learn.microsoft')) return { letter: 'MS', className: 'bg-sky-500/15 text-sky-500' };
  if (key.includes('cloudflare')) return { letter: 'CF', className: 'bg-orange-500/15 text-orange-500' };
  if (key.includes('notion')) return { letter: 'N', className: 'bg-neutral-500/15 text-foreground' };
  if (key.includes('wikipedia')) return { letter: 'W', className: 'bg-stone-500/15 text-stone-500' };
  if (key.includes('arxiv')) return { letter: 'ar', className: 'bg-red-600/15 text-red-600' };
  if (key.includes('mdn')) return { letter: 'M', className: 'bg-neutral-500/15 text-foreground' };

  return null;
}
