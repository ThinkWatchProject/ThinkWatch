import { useTranslation } from 'react-i18next';
import { ShieldCheck, ShieldAlert, Loader2, Cpu, Clock } from 'lucide-react';
import { cn } from '@/lib/utils';

interface Props {
  status: 'idle' | 'fetching' | 'grinding' | 'ready' | 'expired' | 'error';
  tried: number;
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
 * Five states:
 *
 *   - `fetching` — server challenge in flight; spinner.
 *   - `grinding` — Web Worker hashing; emerald background fills
 *                  left-to-right as the CDF of finding a solution
 *                  rises. No numeric counters in the text (kept the
 *                  chip clean) — the bar IS the progress signal.
 *   - `ready`    — solid green shield + "verified".
 *   - `expired`  — amber clock + "expired" + Retry link. The Redis
 *                  challenge died; submitting would hit
 *                  "Proof-of-work challenge expired" anyway. The
 *                  TTL trip is handled by the hook's setTimeout.
 *   - `error`    — red shield + "Retry" link.
 */
export function PowIndicator({
  status,
  tried,
  difficulty,
  errorMessage,
  onRetry,
  className,
}: Props) {
  const { t } = useTranslation();

  const tone =
    status === 'ready'
      ? 'success'
      : status === 'expired'
        ? 'warning'
        : status === 'error'
          ? 'error'
          : 'busy';

  const icon =
    status === 'ready' ? (
      <ShieldCheck className="h-3.5 w-3.5" />
    ) : status === 'expired' ? (
      <Clock className="h-3.5 w-3.5" />
    ) : status === 'error' ? (
      <ShieldAlert className="h-3.5 w-3.5" />
    ) : status === 'fetching' ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin" />
    ) : (
      <Cpu className="h-3.5 w-3.5 animate-pulse" />
    );

  const showRetry = status === 'error' || status === 'expired';

  // PoW solve time is exponentially distributed: P(found by `tried`)
  // = 1 - exp(-tried / 2^difficulty). Using the CDF (not a linear
  // tried/2^d ratio) means the bar smoothly decelerates as it nears
  // the right edge and never overshoots — at 2^d tries it sits at
  // ~63%, at 3·2^d at ~95%, asymptotic to 100%. Cap at 99.5% so the
  // bar never visually "completes" before the worker actually emits
  // `done` (which is when status flips to `ready`).
  const progressPct =
    status === 'grinding' && difficulty > 0
      ? Math.min(99.5, (1 - Math.exp(-tried / 2 ** difficulty)) * 100)
      : null;

  return (
    <div
      className={cn(
        'relative flex items-center gap-2 overflow-hidden rounded-md border px-2.5 py-1.5 text-[11px] font-medium tabular-nums transition-colors',
        tone === 'success' &&
          'border-emerald-500/30 bg-emerald-500/5 text-emerald-700 dark:text-emerald-400',
        tone === 'warning' &&
          'border-amber-500/40 bg-amber-500/5 text-amber-700 dark:text-amber-400',
        tone === 'error' &&
          'border-destructive/40 bg-destructive/5 text-destructive',
        tone === 'busy' &&
          'border-border bg-muted/40 text-muted-foreground',
        className,
      )}
      aria-live="polite"
      aria-atomic="true"
    >
      {progressPct !== null && (
        <div
          className="absolute inset-y-0 left-0 bg-emerald-500/20 transition-[width] duration-200 ease-out dark:bg-emerald-500/25"
          style={{ width: `${progressPct}%` }}
          aria-hidden
        />
      )}
      <span className="relative shrink-0">{icon}</span>
      <span className="relative flex-1 truncate">
        {status === 'fetching' && t('auth.pow.fetching')}
        {status === 'grinding' && t('auth.pow.grinding')}
        {status === 'ready' && t('auth.pow.ready')}
        {status === 'expired' && t('auth.pow.expired')}
        {status === 'error' && (
          <>
            {t('auth.pow.errorShort')}
            {errorMessage && (
              <span className="ml-1 font-mono opacity-70">{errorMessage}</span>
            )}
          </>
        )}
        {status === 'idle' && t('auth.pow.awaiting')}
      </span>
      {showRetry && (
        <button
          type="button"
          onClick={onRetry}
          className="relative ml-1 shrink-0 underline hover:no-underline"
        >
          {t('common.retry')}
        </button>
      )}
    </div>
  );
}
