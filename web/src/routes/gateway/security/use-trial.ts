import { useEffect, useState } from 'react';
import { keepPreviousData, skipToken, useQuery } from '@tanstack/react-query';
import i18n from '@/i18n';
import { api } from '@/lib/api';
import type { Guard, SecurityTestRequest, SecurityTestResult } from '@/lib/security-types';

/** A result and the sample it was computed on — marks only fit that text. */
export interface TrialResult extends SecurityTestResult {
  sample: string;
}

export type Trial =
  | { state: 'idle' }
  /** Waiting for the server. `last` is the previous result, kept on screen meanwhile. */
  | { state: 'running'; last?: TrialResult }
  | { state: 'done'; result: TrialResult }
  | { state: 'failed'; error: string };

/**
 * Tries a sample on the server as it is typed, 250 ms after the last change.
 *
 * The server runs the gateway's own engine — regex dialect, JSON escapes,
 * code-point ranges — so what is marked here is what a real request would
 * hit. `request` is null while there is nothing to try (no sample, or a
 * pattern still empty or malformed).
 */
export function useTrial(guard: Guard, request: SecurityTestRequest | null): Trial {
  const key = request && request.sample.length > 0 ? JSON.stringify(request) : '';
  const [settled, setSettled] = useState(key);
  useEffect(() => {
    const h = setTimeout(() => setSettled(key), 250);
    return () => clearTimeout(h);
  }, [key]);

  const body = settled ? (JSON.parse(settled) as SecurityTestRequest) : null;
  const query = useQuery({
    queryKey: ['admin', 'security', guard, 'test', body],
    queryFn: body
      ? async ({ signal }): Promise<TrialResult> => {
          const result = await api<SecurityTestResult>(`/api/admin/security/${guard}/test`, {
            method: 'POST',
            body,
            signal,
          });
          return { ...result, sample: body.sample };
        }
      : skipToken,
    placeholderData: keepPreviousData,
  });

  if (!key) return { state: 'idle' };
  if (query.isError) {
    return {
      state: 'failed',
      error: query.error instanceof Error ? query.error.message : i18n.t('common.error'),
    };
  }
  if (key !== settled || query.isFetching || query.isPlaceholderData || !query.data) {
    return { state: 'running', last: query.data };
  }
  return { state: 'done', result: query.data };
}
