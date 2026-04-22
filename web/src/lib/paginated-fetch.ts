import { api } from './api';

// Two pagination wire shapes coexist in the codebase:
//  * "items" — `{ items, total }` paged via `?page=&page_size=`. Most
//    handlers added since the limits refactor use this.
//  * "data"  — `{ data, total, per_page }` paged via `?page=&per_page=`,
//    coming from the older `PaginatedResponse<T>` Rust type. The admin
//    user / team list endpoints still use it.
// fetchAllPaginated picks the right query-param + response key based
// on `shape`; default stays "items" since it's the newer convention.
export type PaginationShape = 'items' | 'data';

type ItemsPage<T> = { items: T[]; total: number };
type DataPage<T> = { data: T[]; total: number };

// Fetch every row from a paginated endpoint. The server clamps page
// size (typically to 100 or 200) so callers that need the complete
// set — pickers, permission trees, name lookups — can't rely on a
// single wide request. First page doubles as the total probe;
// remaining pages fan out in parallel.
//
// `baseUrl` may already carry query string params (e.g. a filter);
// the helper appends the size + page params with the right separator.
export async function fetchAllPaginated<T>(
  baseUrl: string,
  pageSize = 200,
  shape: PaginationShape = 'items',
): Promise<T[]> {
  const itemsOf = (page: ItemsPage<T> | DataPage<T>): T[] =>
    shape === 'items' ? (page as ItemsPage<T>).items : (page as DataPage<T>).data;

  const first = await api<ItemsPage<T> | DataPage<T>>(
    withPage(baseUrl, 1, pageSize, shape),
  );
  const firstItems = itemsOf(first);
  if (firstItems.length >= first.total) return firstItems;
  const pageCount = Math.ceil(first.total / pageSize);
  const rest = await Promise.all(
    Array.from({ length: pageCount - 1 }, (_, i) =>
      api<ItemsPage<T> | DataPage<T>>(
        withPage(baseUrl, i + 2, pageSize, shape),
      ).then(itemsOf),
    ),
  );
  return [...firstItems, ...rest.flat()];
}

function withPage(
  baseUrl: string,
  page: number,
  pageSize: number,
  shape: PaginationShape,
): string {
  const sizeParam = shape === 'items' ? 'page_size' : 'per_page';
  const sep = baseUrl.includes('?') ? '&' : '?';
  return `${baseUrl}${sep}page=${page}&${sizeParam}=${pageSize}`;
}
