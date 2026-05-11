import { useTranslation } from 'react-i18next';
import { ShieldCheck, ShieldAlert, Loader2, Cpu } from 'lucide-react';
import { cn } from '@/lib/utils';

interface Props {
  status: 'idle' | 'fetching' | 'grinding' | 'ready' | 'error';
  tried: number;
  elapsedMs: number;
  difficulty: number;
  errorMessage: string | null;
  onRetry: () => void;
  className?: string;
}

/**
 * Compact status chip rendered under the login form. Surfaces the
 * proof-of-work grinder's progress + final state so the security
 * theatre is visible (users see "anti-bot shield engaged" instead of
 * an unexplained delay) without hijacking attention.
 *
 * Four states:
 *
 *   - `fetching` — server challenge in flight; spinner.
 *   - `grinding` — Web Worker hashing; live counter, animated icon.
 *   - `ready`    — solid green shield + "verified" + grind metrics.
 *   - `error`    — red shield + "Retry" link.
 *
 * Designed to feel "high-tech but harmless": monospace numbers,
 * subtle border / muted background, color graduates from neutral
 * → emerald on success.
 */
export function PowIndicator({
  status,
  tried,
  elapsedMs,
  difficulty,
  errorMessage,
  onRetry,
  className,
}: Props) {
  const { t } = useTranslation();

  // The chip is always rendered; the inner content + colors switch.
  // Always-rendering avoids layout jump when status flips.
  const tone =
    status === 'ready'
      ? 'success'
      : status === 'error'
        ? 'error'
        : 'busy';

  const icon =
    status === 'ready' ? (
      <ShieldCheck className="h-3.5 w-3.5" />
    ) : status === 'error' ? (
      <ShieldAlert className="h-3.5 w-3.5" />
    ) : status === 'fetching' ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin" />
    ) : (
      <Cpu className="h-3.5 w-3.5 animate-pulse" />
    );

  return (
    <div
      className={cn(
        'flex items-center gap-2 rounded-md border px-2.5 py-1.5 text-[11px] font-medium tabular-nums transition-colors',
        tone === 'success' &&
          'border-emerald-500/30 bg-emerald-500/5 text-emerald-700 dark:text-emerald-400',
        tone === 'error' &&
          'border-destructive/40 bg-destructive/5 text-destructive',
        tone === 'busy' &&
          'border-border bg-muted/40 text-muted-foreground',
        className,
      )}
      aria-live="polite"
      aria-atomic="true"
    >
      <span className="shrink-0">{icon}</span>
      <span className="flex-1 truncate">
        {status === 'fetching' && t('auth.pow.fetching')}
        {status === 'grinding' && (
          <>
            {t('auth.pow.grinding')}
            <span className="ml-1 font-mono opacity-70">
              {tried.toLocaleString()} hashes
            </span>
          </>
        )}
        {status === 'ready' && (
          <>
            {t('auth.pow.ready')}
            <span className="ml-1 font-mono opacity-60">
              · {difficulty}b · {Math.round(elapsedMs)}ms · {tried.toLocaleString()} hashes
            </span>
          </>
        )}
        {status === 'error' && (
          <>
            {t('auth.pow.errorShort')}
            {errorMessage && (
              <span className="ml-1 font-mono opacity-70">{errorMessage}</span>
            )}
          </>
        )}
        {status === 'idle' && t('auth.pow.fetching')}
      </span>
      {status === 'error' && (
        <button
          type="button"
          onClick={onRetry}
          className="ml-1 shrink-0 underline hover:no-underline"
        >
          {t('common.retry')}
        </button>
      )}
    </div>
  );
}
