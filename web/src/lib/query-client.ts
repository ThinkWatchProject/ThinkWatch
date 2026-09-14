import { QueryClient } from '@tanstack/react-query';

/**
 * Build the query client the console shares. See "Data fetching" in
 * web/README.md for how screens use it.
 *
 * Two defaults differ from the library's, and both keep the load pattern
 * the console had before it went through a cache:
 *
 * · **No retries.** Almost every failure here is a 4xx — a missing
 *   permission, a row someone else deleted — that no retry can fix, and the
 *   library's three silent attempts would hold a spinner up for seconds
 *   before the page admitted the error.
 *
 * · **No refetch on window focus.** Admin pages mount several queries at
 *   once, some of them multi-page catalog loops; refiring all of them on
 *   every tab switch is a change in traffic, not a fix. A screen that needs
 *   fresher data asks for it — `refetchInterval` where it polls, an
 *   invalidation after a write.
 *
 * Cached queries no screen is using are dropped after every successful
 * write — `onSuccessfulWrite` in `api.ts`, wired up in `main.tsx` — so no
 * screen reopens onto data from before a write made in this tab.
 *
 * A factory rather than a singleton, so every test starts from an empty
 * cache.
 */
export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        refetchOnWindowFocus: false,
      },
    },
  });
}
