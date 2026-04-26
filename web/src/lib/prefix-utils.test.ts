import { describe, expect, it } from 'vitest';
import {
  resolveCollision,
  sanitizePrefixInput,
  slugifyPrefix,
} from './prefix-utils';

/**
 * `prefix-utils` is shared between the manual MCP server registration
 * dialog and the store install dialog — both depend on it staying in
 * lock-step with the backend's `^[a-z0-9_]{1,32}$` prefix rule. A
 * regression here either rejects valid input client-side or sends the
 * server something the UNIQUE constraint on `mcp_servers.namespace_prefix`
 * will trip on, leaving the user with a useless 500.
 */

describe('slugifyPrefix', () => {
  it('lowercases, replaces non [a-z0-9_] with underscore, strips edges', () => {
    expect(slugifyPrefix('My GitHub Server')).toBe('my_github_server');
    expect(slugifyPrefix('weather/api/v2')).toBe('weather_api_v2');
    expect(slugifyPrefix('--foo  bar--')).toBe('foo_bar');
  });

  it('caps at 32 characters to match the backend regex', () => {
    const long = 'a'.repeat(50);
    const out = slugifyPrefix(long);
    expect(out.length).toBe(32);
    // The backend rule is ^[a-z0-9_]{1,32}$ — pin both ends so a
    // refactor that forgets the cap fails here, not on the next
    // server roundtrip.
    expect(out).toMatch(/^[a-z0-9_]{1,32}$/);
  });

  it('collapses runs of illegal chars into a single underscore', () => {
    // Reading the implementation: the regex is /[^a-z0-9_]+/g (greedy
    // run), so "@@@" → "_" not "___". Pin the contract because
    // someone might "fix" the regex to a non-greedy match thinking
    // it's safer.
    expect(slugifyPrefix('foo@@@bar')).toBe('foo_bar');
    expect(slugifyPrefix('a    b')).toBe('a_b');
  });

  it('returns empty string when input has no legal characters', () => {
    // Caller is expected to handle empty — we don't substitute a
    // default here (the dialog asks the user to retype).
    expect(slugifyPrefix('!!!')).toBe('');
    expect(slugifyPrefix('___')).toBe('');
  });

  it('preserves digits and underscores already in input', () => {
    expect(slugifyPrefix('aws_v2_2024')).toBe('aws_v2_2024');
  });
});

describe('sanitizePrefixInput', () => {
  it('replaces every illegal char with underscore, keeps length', () => {
    // Differs from slugifyPrefix: this is for live <input> typing,
    // so it must NOT collapse runs (preserves cursor position) and
    // must NOT trim — the user may be mid-word.
    expect(sanitizePrefixInput('Foo Bar')).toBe('foo_bar');
    expect(sanitizePrefixInput('foo  bar')).toBe('foo__bar');
    expect(sanitizePrefixInput('@_test_@')).toBe('__test__');
  });

  it('does not enforce max length (the input element does)', () => {
    const long = 'a'.repeat(50);
    expect(sanitizePrefixInput(long).length).toBe(50);
  });
});

describe('resolveCollision', () => {
  const baseName = 'GitHub';
  const basePrefix = 'github';

  it('returns the base pair when nothing is taken', () => {
    expect(
      resolveCollision(baseName, basePrefix, new Set(), new Set()),
    ).toEqual({ name: 'GitHub', prefix: 'github' });
  });

  it('appends " #2" / "_2" when the base name is taken', () => {
    const out = resolveCollision(
      baseName,
      basePrefix,
      new Set(['GitHub']),
      new Set(['github']),
    );
    expect(out).toEqual({ name: 'GitHub #2', prefix: 'github_2' });
  });

  it('skips numbers that conflict on either side', () => {
    // Name slot 2 free, prefix slot 2 taken → must skip to 3.
    const out = resolveCollision(
      baseName,
      basePrefix,
      new Set(['GitHub']),
      new Set(['github', 'github_2']),
    );
    expect(out).toEqual({ name: 'GitHub #3', prefix: 'github_3' });
  });

  it('returns null after 99 attempts to bound the loop', () => {
    // Pre-fill all 99 slots so the resolver runs out.
    const names = new Set<string>(['GitHub']);
    const prefixes = new Set<string>(['github']);
    for (let i = 2; i < 100; i++) {
      names.add(`GitHub #${i}`);
      prefixes.add(`github_${i}`);
    }
    expect(resolveCollision(baseName, basePrefix, names, prefixes)).toBeNull();
  });
});
