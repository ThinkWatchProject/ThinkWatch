import { useEffect, useState } from 'react';
import { api } from '@/lib/api';
import type { TeamSummary } from '@/lib/types';

export function useTeams() {
  const [teams, setTeams] = useState<TeamSummary[]>([]);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    api<TeamSummary[]>('/api/admin/teams')
      .then(setTeams)
      .catch(() => {})
      .finally(() => setLoading(false));
  }, []);
  return { teams, loading };
}
