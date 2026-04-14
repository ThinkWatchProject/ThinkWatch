import { cn } from '@/lib/utils';

interface ProviderTypeBadgeProps {
  type: string;
  className?: string;
}

const STYLE: Record<string, { bg: string; text: string; ring: string; label: string; letter: string }> = {
  openai: {
    bg: 'bg-emerald-500/10',
    text: 'text-emerald-600 dark:text-emerald-400',
    ring: 'ring-emerald-500/30',
    label: 'OpenAI',
    letter: 'O',
  },
  anthropic: {
    bg: 'bg-amber-500/10',
    text: 'text-amber-600 dark:text-amber-400',
    ring: 'ring-amber-500/30',
    label: 'Anthropic',
    letter: 'A',
  },
  google: {
    bg: 'bg-blue-500/10',
    text: 'text-blue-600 dark:text-blue-400',
    ring: 'ring-blue-500/30',
    label: 'Google',
    letter: 'G',
  },
  azure_openai: {
    bg: 'bg-cyan-500/10',
    text: 'text-cyan-600 dark:text-cyan-400',
    ring: 'ring-cyan-500/30',
    label: 'Azure',
    letter: 'Az',
  },
  bedrock: {
    bg: 'bg-violet-500/10',
    text: 'text-violet-600 dark:text-violet-400',
    ring: 'ring-violet-500/30',
    label: 'Bedrock',
    letter: 'B',
  },
  custom: {
    bg: 'bg-stone-500/10',
    text: 'text-stone-600 dark:text-stone-400',
    ring: 'ring-stone-500/30',
    label: 'Custom',
    letter: 'C',
  },
};

/**
 * Branded provider-type badge. Shows a colored letter token + label.
 * Replaces the generic `Badge` variant lookup so each provider has
 * its own visual identity instead of a generic pill.
 */
export function ProviderTypeBadge({ type, className }: ProviderTypeBadgeProps) {
  const s = STYLE[type] ?? STYLE.custom;
  return (
    <span
      className={cn(
        'inline-flex items-center gap-1.5 rounded-md px-1.5 py-0.5 text-xs font-medium ring-1 ring-inset',
        s.bg,
        s.text,
        s.ring,
        className,
      )}
    >
      <span className="inline-flex h-4 w-4 items-center justify-center rounded bg-current/10 font-mono text-[10px] font-bold">
        {s.letter}
      </span>
      {s.label}
    </span>
  );
}
