import { useEffect, useState } from 'react';
import { api } from '@/lib/api';

interface HealthPayload {
  postgres: boolean;
  redis: boolean;
  clickhouse: boolean;
}

export type SystemStatus = 'operational' | 'degraded' | 'down' | 'unknown';

const POLL_MS = 60_000;

export function useSystemHealth(): SystemStatus {
  const [status, setStatus] = useState<SystemStatus>('unknown');

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const tick = async () => {
      try {
        const h = await api<HealthPayload>('/api/health', {
          signal: controller.signal,
          no401Redirect: true,
        });
        if (cancelled) return;
        const up = [h.postgres, h.redis, h.clickhouse].filter(Boolean).length;
        setStatus(up === 3 ? 'operational' : up === 0 ? 'down' : 'degraded');
      } catch {
        if (cancelled) return;
        setStatus('down');
      }
    };

    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      cancelled = true;
      controller.abort();
      clearInterval(id);
    };
  }, []);

  return status;
}
