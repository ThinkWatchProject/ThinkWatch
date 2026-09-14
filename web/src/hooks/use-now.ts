import { useEffect, useState } from 'react';

/**
 * A timestamp that ticks, for relative-time labels ("due in 12s", "1.5h").
 *
 * Reading `Date.now()` during render makes the render impure: the same props
 * produce a different tree on every call, so neither React nor the compiler —
 * which is allowed to cache render output — has any way to know the label has
 * gone stale. The clock has to be state.
 *
 * **The ticking is the point, not a side effect of satisfying a lint rule.** A
 * countdown that only advances when something else happens to re-render is
 * wrong for most of the time it is on screen, and a stale one is worse than no
 * countdown: it looks live.
 *
 * Pick the interval to match the smallest unit displayed — a label that reads
 * in hours does not need to wake the component up every second.
 */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}
