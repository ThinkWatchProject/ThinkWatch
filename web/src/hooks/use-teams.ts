import { useEffect, useState } from 'react';
import { api } from '@/lib/api';
import type { TeamSummary } from '@/lib/types';

export function useTeams() {
  const [teams, setTeams] = useState<TeamSummary[]>([]);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    const controller = new AbortController();
    api<TeamSummary[]>('/api/admin/teams', { signal: controller.signal })
      .then(setTeams)
      .catch((e) => {
        if (!controller.signal.aborted) console.error(e);
      })
      .finally(() => setLoading(false));
    return () => controller.abort();
  }, []);
  return { teams, loading };
}
