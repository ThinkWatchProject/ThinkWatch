import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Stub the lower-level `api()` helper. We don't care here about
// fetch / cookies / signing — the unit under test is the
// page-arithmetic + parallel-fan-out logic.
const apiMock = vi.fn();

vi.mock('./api', () => ({
  api: (path: string) => apiMock(path),
}));

import { fetchAllPaginated } from './paginated-fetch';

/**
 * `fetchAllPaginated` powers every "load all rows" caller in the
 * frontend (permission tree, model picker, team picker, name
 * lookup). The two pagination shapes — `{items, total}` (newer)
 * and `{data, total}` (older PaginatedResponse) — must round-trip
 * through the same call site without the caller knowing which the
 * server speaks. Pin both shapes here.
 */

describe('fetchAllPaginated', () => {
  beforeEach(() => {
    apiMock.mockReset();
  });

  afterEach(() => {
    apiMock.mockReset();
  });

  it('returns the first page directly when total ≤ pageSize', () => {
    apiMock.mockResolvedValueOnce({ items: [{ id: 1 }, { id: 2 }], total: 2 });
    return fetchAllPaginated<{ id: number }>('/api/things', 200).then((all) => {
      expect(all).toEqual([{ id: 1 }, { id: 2 }]);
      expect(apiMock).toHaveBeenCalledTimes(1);
      expect(apiMock).toHaveBeenCalledWith('/api/things?page=1&page_size=200');
    });
  });

  it('fans out remaining pages in parallel, preserving order', async () => {
    // total=5, pageSize=2 → pages [1,2], [3,4], [5]. After page 1
    // the helper computes pageCount=3 and dispatches pages 2 + 3
    // in parallel.
    apiMock
      .mockResolvedValueOnce({ items: [{ id: 1 }, { id: 2 }], total: 5 })
      .mockResolvedValueOnce({ items: [{ id: 3 }, { id: 4 }], total: 5 })
      .mockResolvedValueOnce({ items: [{ id: 5 }], total: 5 });

    const all = await fetchAllPaginated<{ id: number }>('/api/things', 2);
    expect(all.map((x) => x.id)).toEqual([1, 2, 3, 4, 5]);
    expect(apiMock).toHaveBeenCalledTimes(3);
    // Page 1 first; the rest are unordered (Promise.all), so just
    // assert the call set.
    const calls = apiMock.mock.calls.map((c: unknown[]) => c[0]);
    expect(calls[0]).toBe('/api/things?page=1&page_size=2');
    expect(new Set(calls)).toEqual(
      new Set([
        '/api/things?page=1&page_size=2',
        '/api/things?page=2&page_size=2',
        '/api/things?page=3&page_size=2',
      ]),
    );
  });

  it("speaks the older 'data' shape when asked", async () => {
    apiMock
      .mockResolvedValueOnce({ data: [{ id: 1 }, { id: 2 }], total: 3 })
      .mockResolvedValueOnce({ data: [{ id: 3 }], total: 3 });
    const all = await fetchAllPaginated<{ id: number }>(
      '/api/admin/users',
      2,
      'data',
    );
    expect(all.map((x) => x.id)).toEqual([1, 2, 3]);
    // `data` shape uses `per_page`, NOT `page_size`. The two query
    // param names are not interchangeable on the server side — pin
    // the right one.
    const calls = apiMock.mock.calls.map((c: unknown[]) => c[0]);
    expect(calls[0]).toBe('/api/admin/users?page=1&per_page=2');
    expect(calls).toContain('/api/admin/users?page=2&per_page=2');
  });

  it('appends with `&` when baseUrl already carries a query string', async () => {
    apiMock.mockResolvedValueOnce({ items: [], total: 0 });
    await fetchAllPaginated('/api/things?filter=enabled', 100);
    expect(apiMock).toHaveBeenCalledWith(
      '/api/things?filter=enabled&page=1&page_size=100',
    );
  });

  it('returns empty array when the first page has zero total', async () => {
    apiMock.mockResolvedValueOnce({ items: [], total: 0 });
    const all = await fetchAllPaginated<{ id: number }>('/api/things', 50);
    expect(all).toEqual([]);
    expect(apiMock).toHaveBeenCalledTimes(1);
  });

  it('does not refetch when the first page already covers the total exactly', async () => {
    // pageSize 100, total 100 — server returned everything in one
    // page. The helper must NOT issue a redundant page=2 fetch.
    apiMock.mockResolvedValueOnce({
      items: Array.from({ length: 100 }, (_, i) => ({ id: i })),
      total: 100,
    });
    const all = await fetchAllPaginated<{ id: number }>('/api/things', 100);
    expect(all).toHaveLength(100);
    expect(apiMock).toHaveBeenCalledTimes(1);
  });
});
