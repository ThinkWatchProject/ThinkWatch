// Split out of `stat-cards.tsx` so that file exports only components:
// Fast Refresh gives up on a module that mixes the two, and losing HMR on
// the dashboard cards is a real cost while iterating on them.

// 12s is past every realistic CH analytics query (P99 < 3s on the
// existing dashboards) but short enough that a frozen result lands
// the per-card error fallback well before a user gives up scrolling.
export const DASHBOARD_CARD_TIMEOUT_MS = 12_000;

/**
 * Race a promise against a deadline; on timeout reject with a
 * labelled Error that the ErrorBoundary surfaces. The cleared timer
 * keeps the JS heap clean when the underlying request resolves first.
 */
export function withTimeout<T>(p: Promise<T>, ms: number, label: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const id = setTimeout(() => {
      reject(new Error(`Timed out fetching ${label} (>${ms}ms)`));
    }, ms);
    p.then(
      (v) => {
        clearTimeout(id);
        resolve(v);
      },
      (e) => {
        clearTimeout(id);
        reject(e);
      },
    );
  });
}
