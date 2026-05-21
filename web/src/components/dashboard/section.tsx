import type { ReactNode } from 'react';

/**
 * Eyebrow + content wrapper used by every dashboard panel.
 *
 * The child fills the remaining vertical space when the parent flexes
 * (so the internal scroll lists can shrink to fit). An optional
 * `action` slot is rendered right-aligned next to the eyebrow — e.g.
 * the live-log pause toggle or the provider filter tabs.
 */
export function Section({
  eyebrow,
  action,
  className,
  children,
}: {
  eyebrow: string;
  action?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`flex min-h-0 flex-col ${className ?? ''}`}>
      <div className="mb-2 flex shrink-0 items-center justify-between gap-3">
        <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-muted-foreground">
          {eyebrow}
        </div>
        {action}
      </div>
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}
