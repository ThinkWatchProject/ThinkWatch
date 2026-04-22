import { api } from './api';

interface Page<T> {
  items: T[];
  total: number;
}

// Fetch every row from a `{ items, total }` paginated endpoint. The
// server clamps page_size (typically to 200) so callers that need the
// complete set — pickers, permission trees, name lookups — can't rely
// on a single wide request. First page doubles as the total probe;
// remaining pages fan out in parallel.
//
// `baseUrl` may already carry query string params (e.g. a filter); the
// helper appends `page` / `page_size` with the right separator.
export async function fetchAllPaginated<T>(
  baseUrl: string,
  pageSize = 200,
): Promise<T[]> {
  const first = await api<Page<T>>(withPage(baseUrl, 1, pageSize));
  if (first.items.length >= first.total) return first.items;
  const pageCount = Math.ceil(first.total / pageSize);
  const rest = await Promise.all(
    Array.from({ length: pageCount - 1 }, (_, i) =>
      api<Page<T>>(withPage(baseUrl, i + 2, pageSize)).then((r) => r.items),
    ),
  );
  return [...first.items, ...rest.flat()];
}

function withPage(baseUrl: string, page: number, pageSize: number): string {
  const sep = baseUrl.includes('?') ? '&' : '?';
  return `${baseUrl}${sep}page=${page}&page_size=${pageSize}`;
}
