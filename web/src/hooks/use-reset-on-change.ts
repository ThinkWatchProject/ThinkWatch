import { useState } from 'react';

/**
 * Run `reset` during render whenever `key` changes.
 *
 * This is React's documented way to adjust state when a prop or a derived
 * value changes (https://react.dev/learn/you-might-not-need-an-effect), and
 * it is not the same thing as doing it in an effect:
 *
 * · An effect resets **after** the browser has painted, so the old value is
 *   on screen for a frame. On a search box that resets the page number, that
 *   frame is long enough to fire a request for page 4 of a result set that
 *   now has two pages — and then race its response against the correct one.
 *
 * · Adjusting during render re-runs this component before anything is
 *   committed. Nothing renders with the stale pair, and no effect keyed on
 *   the stale value ever runs.
 *
 * `Object.is` is the comparison, so pass a primitive (or a value that is
 * referentially stable) as `key` — an object literal rebuilt every render
 * would reset on every render.
 */
export function useResetOnChange<T>(key: T, reset: () => void): void {
  const [seen, setSeen] = useState(key);
  if (!Object.is(seen, key)) {
    setSeen(key);
    reset();
  }
}
