/**
 * Query-syntax parser for the unified log explorer.
 *
 * Input  : `level:error target:auth -path:/health some free text`
 * Output : { params: { level:"error", target:"auth", q:"some free text" },
 *            excludes: ["path:/health"] }
 *
 * Both halves of this module are pure — no DOM, no API. They live in
 * a sibling file so vitest can test them without booting jsdom or the
 * React tree the rest of `logs.tsx` carries.
 */

export function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export interface ParsedQuery {
  /** Positive filters: key=value or `q` for free text. */
  params: Record<string, string>;
  /** Negative filters from `-key:value` tokens, kept as raw `key:value` strings. */
  excludes: string[];
}

export function parseQuery(input: string): ParsedQuery {
  const params: Record<string, string> = {};
  const excludes: string[] = [];
  const freeText: string[] = [];
  // Match optional leading "-" then key:value (value can be quoted)
  const regex = /(-?)(\w+):(?:"([^"]*)"|(\S+))/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = regex.exec(input)) !== null) {
    // Collect text before this match as free text
    const before = input.slice(lastIndex, match.index).trim();
    if (before) freeText.push(before);
    lastIndex = regex.lastIndex;
    const negate = match[1] === '-';
    const key = match[2];
    const value = match[3] ?? match[4];
    if (negate) {
      // Re-quote if the value contains anything that would confuse the
      // backend splitter (whitespace, comma, colon, quote, backslash).
      // Inside the quotes, escape `\` and `"` so the backend's escape-aware
      // splitter can recover the original value.
      const needsQuotes = /[\s,:"\\]/.test(value);
      const serialized = needsQuotes
        ? `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`
        : value;
      excludes.push(`${key}:${serialized}`);
    } else {
      params[key] = value;
    }
  }
  const after = input.slice(lastIndex).trim();
  if (after) freeText.push(after);
  if (freeText.length > 0) params.q = freeText.join(' ');
  return { params, excludes };
}

/**
 * Strip the first `key:value` (optionally `-key:value`) token from a raw
 * query string. Used when the user clicks × on a parsed chip. Matches the
 * same shape as `parseQuery` so quoted values come out cleanly.
 */
export function removeFilterToken(input: string, key: string, negate: boolean): string {
  const prefix = negate ? '-' : '';
  const pattern = new RegExp(
    `(^|\\s)${escapeRegex(prefix)}${escapeRegex(key)}:(?:"[^"]*"|\\S+)`,
  );
  return input.replace(pattern, '').replace(/\s+/g, ' ').trim();
}
