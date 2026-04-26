import { describe, expect, it } from 'vitest';
import { parseQuery, removeFilterToken } from './query-parser';

/**
 * The unified log explorer's search bar is a tiny lucene-ish DSL:
 *   `key:value` → positive filter
 *   `-key:value` → negative filter (excluded)
 *   anything else → free text accumulated into `q`
 *
 * This parser is the only thing standing between the user's typed
 * query and the URL the table fetches against. A regression here
 * shows up as "filter chip looks right but results don't change",
 * which is exactly the kind of bug nobody files a ticket for.
 */

describe('parseQuery', () => {
  it('returns empty params for empty input', () => {
    expect(parseQuery('')).toEqual({ params: {}, excludes: [] });
  });

  it('extracts a single key:value pair', () => {
    const out = parseQuery('level:error');
    expect(out.params).toEqual({ level: 'error' });
    expect(out.excludes).toEqual([]);
  });

  it('extracts multiple key:value pairs', () => {
    const out = parseQuery('level:error target:auth status_code:500');
    expect(out.params).toEqual({
      level: 'error',
      target: 'auth',
      status_code: '500',
    });
    expect(out.excludes).toEqual([]);
  });

  it('collects free text into a `q` parameter', () => {
    const out = parseQuery('connection refused');
    expect(out.params).toEqual({ q: 'connection refused' });
  });

  it('mixes named filters with free text in any order', () => {
    // Free text BEFORE and AFTER a key:value still concatenates into
    // a single `q`. Pin so a refactor that drops one side stays out.
    const out = parseQuery('hello level:error there world');
    expect(out.params).toEqual({ level: 'error', q: 'hello there world' });
  });

  it('handles quoted values containing whitespace', () => {
    const out = parseQuery('path:"/api/admin users"');
    expect(out.params).toEqual({ path: '/api/admin users' });
  });

  it('routes -key:value tokens into excludes, not params', () => {
    const out = parseQuery('-level:debug -path:/health');
    expect(out.params).toEqual({});
    // Plain values pass through verbatim — only when the value
    // contains shell-special chars do we re-quote.
    expect(out.excludes).toEqual(['level:debug', 'path:/health']);
  });

  it('re-quotes excluded values that contain whitespace, comma, or colon', () => {
    // The backend's exclude splitter treats commas as separators, so
    // anything that looks like a delimiter has to be wrapped to
    // round-trip cleanly. (Embedded quotes / backslashes inside an
    // already-quoted value are not currently supported by the
    // top-level regex — its `[^"]*` alternative stops at the first
    // inner quote — that's a parser limitation, not what this test
    // is pinning.)
    const out = parseQuery('-tag:"a,b" -path:"with space"');
    expect(out.excludes).toEqual(['tag:"a,b"', 'path:"with space"']);
  });

  it('does not promote `-key:value` to `q` even alongside free text', () => {
    const out = parseQuery('thing happened -level:debug now');
    expect(out.params).toEqual({ q: 'thing happened now' });
    expect(out.excludes).toEqual(['level:debug']);
  });

  it('keeps the LAST occurrence of duplicate keys', () => {
    // Backend filter is single-valued per key. Pin "last write
    // wins" so the parsed chip the UI shows actually matches the
    // request the table fires.
    const out = parseQuery('level:info level:error');
    expect(out.params.level).toBe('error');
  });
});

describe('removeFilterToken', () => {
  it('strips the named positive token, preserving the rest', () => {
    expect(removeFilterToken('level:error target:auth', 'level', false)).toBe(
      'target:auth',
    );
  });

  it('strips a negative token only when negate=true', () => {
    expect(removeFilterToken('-level:debug target:auth', 'level', true)).toBe(
      'target:auth',
    );
    // negate=false must NOT match the negated token.
    expect(removeFilterToken('-level:debug target:auth', 'level', false)).toBe(
      '-level:debug target:auth',
    );
  });

  it('strips a quoted value cleanly', () => {
    expect(
      removeFilterToken('path:"/api/v1 admin" status:500', 'path', false),
    ).toBe('status:500');
  });

  it('collapses extra whitespace left behind', () => {
    // `level:debug` between two free-text words → after removal the
    // result must NOT have a double space, because the round-tripped
    // string is what the user sees in the input box.
    expect(
      removeFilterToken('hello level:debug world', 'level', false),
    ).toBe('hello world');
  });

  it('leaves the input untouched when the key is not present', () => {
    expect(removeFilterToken('level:error', 'target', false)).toBe(
      'level:error',
    );
  });
});
