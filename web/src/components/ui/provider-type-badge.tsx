import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';

interface ProviderTypeBadgeProps {
  type: string;
  className?: string;
}

const SPEC: Record<string, { tint: string; label: string }> = {
  openai: {
    tint: 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
    label: 'OpenAI',
  },
  anthropic: {
    tint: 'bg-amber-500/10 text-amber-600 dark:text-amber-400',
    label: 'Anthropic',
  },
  google: {
    tint: 'bg-blue-500/10 text-blue-600 dark:text-blue-400',
    label: 'Google',
  },
  azure_openai: {
    tint: 'bg-cyan-500/10 text-cyan-600 dark:text-cyan-400',
    label: 'Azure',
  },
  bedrock: {
    tint: 'bg-violet-500/10 text-violet-600 dark:text-violet-400',
    label: 'Bedrock',
  },
  custom: {
    tint: 'bg-stone-500/10 text-stone-600 dark:text-stone-400',
    label: 'Custom',
  },
};

/** Tinted pill for a provider type. Per-provider color tint carries the
 *  visual identity; the label carries the name. No lettermark — in a
 *  mono/narrow font the "O" for OpenAI was getting read as a zero. */
export function ProviderTypeBadge({ type, className }: ProviderTypeBadgeProps) {
  const spec = SPEC[type] ?? SPEC.custom;
  return (
    <Badge variant="secondary" className={cn(spec.tint, className)}>
      {spec.label}
    </Badge>
  );
}
