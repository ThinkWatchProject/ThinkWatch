import { useEffect, useState } from 'react';
import { api } from '@/lib/api';

interface HealthPayload {
  postgres: boolean;
  redis: boolean;
  // null when ClickHouse isn't configured for this deployment; in that
  // case it must NOT count against system health.
  clickhouse: boolean | null;
  // null when no S3-compatible backend is wired in (oversize bodies
  // truncate instead of offload); same not-counted-against rule.
  s3: boolean | null;
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
        const services: boolean[] = [h.postgres, h.redis];
        if (h.clickhouse !== null) services.push(h.clickhouse);
        if (h.s3 !== null) services.push(h.s3);
        const up = services.filter(Boolean).length;
        setStatus(up === services.length ? 'operational' : up === 0 ? 'down' : 'degraded');
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
