import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { FieldSaveState } from './useFieldAutosave';

export function useGatewayForm<T>(initial: T, persist: (value: T) => Promise<void>, loadLatest?: () => Promise<T>) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState(initial);
  const [saved, setSaved] = useState(initial);
  const [state, setState] = useState<FieldSaveState>('idle');
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);

  function update(value: T) {
    setDraft(value);
    if (!conflict) {
      setState('idle');
      setError(null);
    }
  }

  async function save() {
    setError(null);
    setState('saving');
    try {
      await persist(draft);
      setSaved(draft);
      setState('saved');
    } catch (failure) {
      const stale = typeof failure === 'object' && failure !== null && 'status' in failure && failure.status === 409;
      setConflict(stale);
      setError(stale ? t('settings.gatewayConflict') : failure instanceof Error ? failure.message : t('admin.settings.saveFailed'));
      setState('error');
    }
  }

  async function reload() {
    if (!loadLatest) return;
    setState('saving');
    try {
      const latest = await loadLatest();
      setDraft(latest);
      setSaved(latest);
      setConflict(false);
      setError(null);
      setState('idle');
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : t('admin.settings.saveFailed'));
      setState('error');
    }
  }

  return { draft, update, state, error, busy: state === 'saving', dirty: draft !== saved, save, conflict, reload };
}
