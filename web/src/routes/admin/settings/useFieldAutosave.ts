import { useEffect, useRef, useState } from 'react';

export type FieldSaveState = 'idle' | 'saving' | 'saved' | 'error';

interface UseFieldAutosaveOpts<T> {
  /** Current value from the page's form state. */
  value: T;
  /** True once the initial GET has populated the form — prevents the hook
   *  from firing a PATCH the moment the page finishes loading. */
  isLoaded: boolean;
  /** Called when the hook decides the value differs from the last-persisted
   *  value. Should resolve on success, throw on failure. */
  persist: (value: T) => Promise<void>;
  /** Debounce window in ms. Text / number → ~600ms; switches and selects
   *  pass 0 so the save fires as soon as the user commits. */
  debounceMs?: number;
}

/// Inline-save for a single scalar setting. The hook watches `value`, and
/// when it diverges from the last-persisted snapshot it schedules a
/// debounced PATCH. State transitions: idle → saving → saved → idle (after
/// a short acknowledgement window) or → error (which sticks until the
/// next successful save).
export function useFieldAutosave<T>({
  value,
  isLoaded,
  persist,
  debounceMs = 600,
}: UseFieldAutosaveOpts<T>): { state: FieldSaveState; error: string | null } {
  const [state, setState] = useState<FieldSaveState>('idle');
  const [error, setError] = useState<string | null>(null);

  // `persist` gets re-created on every render from the caller (inline
  // arrow), so we capture it in a ref instead of depending on it in the
  // effect — otherwise the timer would restart every render and never
  // actually fire.
  const persistRef = useRef(persist);
  persistRef.current = persist;

  // Snapshot of the last value the server acknowledged. Seeded on first
  // load so the hook doesn't interpret "page just populated" as a change.
  const lastSavedRef = useRef<T | null>(null);
  const seededRef = useRef(false);

  useEffect(() => {
    if (!isLoaded) return;
    if (!seededRef.current) {
      lastSavedRef.current = value;
      seededRef.current = true;
      return;
    }
    if (value === lastSavedRef.current) {
      // Reverted to the saved value — cancel any pending acknowledgement
      // so the field doesn't flicker a stale ✓.
      setState('idle');
      setError(null);
      return;
    }

    const id = setTimeout(async () => {
      setState('saving');
      setError(null);
      try {
        await persistRef.current(value);
        lastSavedRef.current = value;
        setState('saved');
        // Clear the ✓ after a beat so the row settles back to calm.
        setTimeout(() => {
          setState((s) => (s === 'saved' ? 'idle' : s));
        }, 1500);
      } catch (err) {
        setState('error');
        setError(err instanceof Error ? err.message : 'Save failed');
      }
    }, debounceMs);

    return () => clearTimeout(id);
  }, [value, isLoaded, debounceMs]);

  return { state, error };
}
