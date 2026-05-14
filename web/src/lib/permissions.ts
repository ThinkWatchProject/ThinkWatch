/** Match a single permission key against a search query. The query
 *  is treated as a literal substring unless it contains `*`, in
 *  which case it's compiled to a regex anchored at start/end. This
 *  lets the admin type `*:delete` to find every key with any
 *  `:delete` action, or `providers:*` to find every key that
 *  touches providers.
 *
 *  Case folding is the caller's responsibility — match against the
 *  raw key when you want case-sensitive matching, lowercase both
 *  sides when you don't.
 */
export function matchPermission(perm: string, query: string): boolean {
  if (!query.includes('*')) return perm.includes(query);
  const escaped = query.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
  try {
    return new RegExp(`^${escaped}$`).test(perm);
  } catch {
    return false;
  }
}
