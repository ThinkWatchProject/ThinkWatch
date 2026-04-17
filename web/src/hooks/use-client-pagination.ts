import { useMemo, useState } from 'react';

/**
 * Slice an in-memory array into pages. Use for tables whose backend
 * returns the full list (config tables, small collections). For
 * server-paginated endpoints, wire the backend's page/per_page params
 * directly into `DataTablePagination` instead.
 */
export function useClientPagination<T>(items: T[], initialPageSize = 20) {
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(initialPageSize);

  const total = items.length;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  // Clamp so a page-size bump or an item deletion can't strand the user
  // on an empty page past the new last page.
  const safePage = Math.min(Math.max(1, page), totalPages);

  const paginated = useMemo(() => {
    const start = (safePage - 1) * pageSize;
    return items.slice(start, start + pageSize);
  }, [items, safePage, pageSize]);

  return {
    paginated,
    total,
    page: safePage,
    pageSize,
    setPage,
    setPageSize,
  };
}
