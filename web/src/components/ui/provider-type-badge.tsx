import { Badge } from '@/components/ui/badge';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import { cn } from '@/lib/utils';

interface ProviderTypeBadgeProps {
  type: string;
  className?: string;
}

const SPEC: Record<string, { tint: string; label: string; letter: string }> = {
  openai: {
    tint: 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
    label: 'OpenAI',
    letter: 'O',
  },
  anthropic: {
    tint: 'bg-amber-500/10 text-amber-600 dark:text-amber-400',
    label: 'Anthropic',
    letter: 'A',
  },
  google: {
    tint: 'bg-blue-500/10 text-blue-600 dark:text-blue-400',
    label: 'Google',
    letter: 'G',
  },
  azure_openai: {
    tint: 'bg-cyan-500/10 text-cyan-600 dark:text-cyan-400',
    label: 'Azure',
    letter: 'Az',
  },
  bedrock: {
    tint: 'bg-violet-500/10 text-violet-600 dark:text-violet-400',
    label: 'Bedrock',
    letter: 'B',
  },
  custom: {
    tint: 'bg-stone-500/10 text-stone-600 dark:text-stone-400',
    label: 'Custom',
    letter: 'C',
  },
};

/**
 * Branded provider chip — shadcn `Avatar` for the circular lettermark,
 * shadcn `Badge` for the pill shell, a per-provider tint className for
 * the accent color. Stays aligned with design tokens while carrying
 * brand-adjacent identity.
 */
export function ProviderTypeBadge({ type, className }: ProviderTypeBadgeProps) {
  const spec = SPEC[type] ?? SPEC.custom;
  return (
    <Badge variant="secondary" className={cn(spec.tint, className)}>
      <Avatar className="h-4 w-4 rounded">
        <AvatarFallback className="rounded bg-current/10 font-mono text-[9px] font-bold">
          {spec.letter}
        </AvatarFallback>
      </Avatar>
      {spec.label}
    </Badge>
  );
}
