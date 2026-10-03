import { useRef, type KeyboardEvent, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SegmentedOption<T extends string> {
  value: T;
  label: ReactNode;
}

/**
 * One choice out of a few, as joined buttons — the same look as the time
 * range pickers on the dashboard and the costs page.
 *
 * A radio group to assistive technology: Tab reaches the chosen option, and
 * the arrow keys move the choice.
 */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  disabled = false,
  size = 'default',
  className,
}: {
  value: T;
  options: readonly SegmentedOption<T>[];
  onChange: (value: T) => void;
  /** Accessible name of the group. */
  label: string;
  disabled?: boolean;
  size?: 'sm' | 'default';
  className?: string;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const at = options.findIndex((o) => o.value === value);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0;
    if (step === 0 || disabled || options.length === 0) return;
    e.preventDefault();
    const next = (Math.max(at, 0) + step + options.length) % options.length;
    refs.current[next]?.focus();
    onChange(options[next].value);
  };

  return (
    <div
      role="radiogroup"
      aria-label={label}
      aria-disabled={disabled || undefined}
      onKeyDown={onKeyDown}
      className={cn(
        'inline-flex items-center gap-0.5 rounded-md border bg-muted/30 p-0.5',
        size === 'sm' ? 'text-xs' : 'text-sm',
        className,
      )}
    >
      {options.map((o, i) => {
        const checked = o.value === value;
        return (
          <button
            key={o.value}
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="radio"
            aria-checked={checked}
            tabIndex={checked || (at < 0 && i === 0) ? 0 : -1}
            disabled={disabled}
            onClick={() => !checked && onChange(o.value)}
            className={cn(
              'rounded px-2.5 font-medium whitespace-nowrap transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50',
              size === 'sm' ? 'py-0.5' : 'py-1',
              checked
                ? 'bg-background text-foreground shadow-sm'
                : 'text-muted-foreground hover:text-foreground',
            )}
          >
            {o.label}
          </button>
        );
      })}
    </div>
  );
}
