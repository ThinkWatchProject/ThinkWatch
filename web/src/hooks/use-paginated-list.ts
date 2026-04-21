import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '@/lib/api';

/**
 * Paginated admin-table response envelope. Matches every server-
 * paginated endpoint we currently expose (`list_users`, `list_keys`,
 * `list_roles`, `list_outbox`, ...). If a new endpoint shapes differently,
 * add a transformer parameter rather than widening this type.
 */
export interface PaginatedResponse<T> {
  data: T[];
  total: number;
}

export interface UsePaginatedListOptions {
  /** Initial items-per-page. Default 20. */
  pageSize?: number;
  /** Debounce window for the search text, in ms. Default 250. */
  searchDebounceMs?: number;
  /** Extra query params stringified into the request. Stable reference
   *  recommended — the effect re-runs when this changes. */
  extraParams?: Record<string, string>;
}

/**
 * Hook that wraps the standard admin-table pattern:
 *
 *   - `page` / `pageSize` state with setters
 *   - debounced `search` text (auto-reset to page 1 on change)
 *   - request dispatch via `api()` with an `AbortSignal` so a fast
 *     typer doesn't race the previous fetch
 *   - `loading` / `error` signals wired to the UI
 *   - `refetch()` for mutations that need to re-pull
 *
 * Routes that need extra sibling requests (roles list, permissions
 * catalog, super-admin ids in `admin/users`) should keep those as
 * separate `useEffect`s — this hook owns ONLY the paginated primary
 * list so the responsibility is narrow.
 */
export function usePaginatedList<T>(
  path: string,
  options: UsePaginatedListOptions = {},
) {
  const { pageSize: initialPageSize = 20, searchDebounceMs = 250, extraParams } =
    options;

  const [items, setItems] = useState<T[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(initialPageSize);
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Debounce the search box so we don't spam the backend per keystroke.
  useEffect(() => {
    const h = setTimeout(() => setDebouncedSearch(search.trim()), searchDebounceMs);
    return () => clearTimeout(h);
  }, [search, searchDebounceMs]);

  // Typing a new search term should always land on page 1 — staying on
  // page 4 of a filtered result with 2 pages is confusing.
  useEffect(() => {
    setPage(1);
  }, [debouncedSearch]);

  // The search/extraParams objects stabilise across re-renders by
  // being stringified into the query URL; reading them into a ref
  // avoids churn when the effect closure captures them.
  const extraParamsRef = useRef(extraParams);
  useEffect(() => {
    extraParamsRef.current = extraParams;
  }, [extraParams]);

  const [refetchTrigger, setRefetchTrigger] = useState(0);
  const refetch = useCallback(() => setRefetchTrigger((n) => n + 1), []);

  useEffect(() => {
    const controller = new AbortController();
    const load = async () => {
      setLoading(true);
      try {
        const params = new URLSearchParams({
          page: String(page),
          per_page: String(pageSize),
        });
        if (debouncedSearch) params.set('search', debouncedSearch);
        for (const [k, v] of Object.entries(extraParamsRef.current ?? {})) {
          params.set(k, v);
        }
        const sep = path.includes('?') ? '&' : '?';
        const res = await api<PaginatedResponse<T>>(`${path}${sep}${params.toString()}`, {
          signal: controller.signal,
        });
        setItems(res.data);
        setTotal(res.total);
        setError(null);
      } catch (err) {
        if (controller.signal.aborted) return;
        setError(err instanceof Error ? err.message : 'Failed to load');
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    };
    load();
    return () => controller.abort();
  }, [path, page, pageSize, debouncedSearch, refetchTrigger]);

  return {
    items,
    total,
    page,
    pageSize,
    search,
    loading,
    error,
    setPage,
    setPageSize,
    setSearch,
    refetch,
  };
}
