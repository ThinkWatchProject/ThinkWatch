/**
 * Helpers for MCP server namespace prefixes — kept in one place so the
 * registration dialog and the store install dialog behave identically
 * and stay in sync with the backend's `normalize_namespace_prefix`.
 */

/**
 * Turn an arbitrary display name into a valid prefix candidate.
 * Matches the backend's rule: `^[a-z0-9_]{1,32}$`.
 */
export function slugifyPrefix(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9_]+/g, '_')
    .replace(/^_+|_+$/g, '')
    .slice(0, 32);
}

/**
 * Append `_2`, `_3`, … to both name and prefix until neither collides
 * with the `taken` entries. Stops at 99 to avoid infinite loops.
 *
 * Returns the first available pair, or `null` if > 99 suffixes are in use.
 */
export function resolveCollision(
  baseName: string,
  basePrefix: string,
  takenNames: Set<string>,
  takenPrefixes: Set<string>,
): { name: string; prefix: string } | null {
  for (let i = 1; i < 100; i++) {
    const name = i === 1 ? baseName : `${baseName} #${i}`;
    const prefix = i === 1 ? basePrefix : `${basePrefix}_${i}`;
    if (!takenNames.has(name) && !takenPrefixes.has(prefix)) {
      return { name, prefix };
    }
  }
  return null;
}

/**
 * Sanitize partial user input as they type — keeps only prefix-legal
 * characters. Does not enforce length (the `maxLength` attribute does that).
 */
export function sanitizePrefixInput(raw: string): string {
  return raw.toLowerCase().replace(/[^a-z0-9_]/g, '_');
}
