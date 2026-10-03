import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

/** A span to mark, in UTF-16 code units — the same indices JavaScript strings use. */
export interface Mark {
  start: number;
  end: number;
  /** `bad`: the request or call would be stopped; `warn`: replaced, deleted or recorded. */
  tone: 'warn' | 'bad';
}

const TONE: Record<Mark['tone'], string> = {
  warn: 'bg-amber-500/20 shadow-[inset_0_-1.5px_0_0_var(--color-amber-500)]',
  bad: 'bg-destructive/15 shadow-[inset_0_-1.5px_0_0_var(--destructive)]',
};

/**
 * A sample with the server's hits marked.
 *
 * **The positions come from the server**, which runs the same engine as the
 * gateway. Matching again in the browser could mark something else: the
 * JavaScript and Rust regex dialects are not the same.
 *
 * Overlapping marks merge in order; a later mark only covers what an earlier
 * one left.
 */
export function Highlight({ text, marks, className }: { text: string; marks: Mark[]; className?: string }) {
  const parts: ReactNode[] = [];
  let at = 0;
  for (const m of [...marks].sort((a, b) => a.start - b.start)) {
    const start = Math.max(m.start, at);
    const end = Math.min(m.end, text.length);
    if (end <= start) continue;
    if (start > at) parts.push(text.slice(at, start));
    parts.push(
      <mark key={start} className={cn('rounded-[3px] text-foreground box-decoration-clone', TONE[m.tone])}>
        {text.slice(start, end)}
      </mark>,
    );
    at = end;
  }
  if (at < text.length) parts.push(text.slice(at));
  return (
    <div className={cn('font-mono text-xs leading-relaxed break-words whitespace-pre-wrap', className)}>
      {parts}
    </div>
  );
}
