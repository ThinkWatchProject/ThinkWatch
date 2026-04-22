import { useEffect, useState } from 'react';
import { API_BASE } from '@/lib/api';

interface SsoStatus {
  ssoEnabled: boolean;
  allowRegistration: boolean;
  loading: boolean;
}

interface SsoStatusResponse {
  enabled: boolean;
  allow_registration?: boolean;
}

// Module-level cache so the fetch only happens once across all consumers.
let cached: SsoStatusResponse | null = null;
let inflight: Promise<SsoStatusResponse> | null = null;

function fetchSsoStatus(): Promise<SsoStatusResponse> {
  if (cached) return Promise.resolve(cached);
  if (inflight) return inflight;
  inflight = fetch(`${API_BASE}/api/auth/sso/status`)
    .then((r) => r.json())
    .then((data: SsoStatusResponse) => {
      cached = data;
      inflight = null;
      return data;
    })
    .catch((err) => {
      console.error(err);
      inflight = null;
      const fallback: SsoStatusResponse = { enabled: false };
      return fallback;
    });
  return inflight;
}

export function useSsoStatus(): SsoStatus {
  const [status, setStatus] = useState<SsoStatus>(() => {
    if (cached) {
      return {
        ssoEnabled: cached.enabled,
        allowRegistration: cached.allow_registration === true,
        loading: false,
      };
    }
    return { ssoEnabled: false, allowRegistration: false, loading: true };
  });

  useEffect(() => {
    if (cached) return;
    let cancelled = false;
    fetchSsoStatus().then((data) => {
      if (cancelled) return;
      setStatus({
        ssoEnabled: data.enabled,
        allowRegistration: data.allow_registration === true,
        loading: false,
      });
    });
    return () => { cancelled = true; };
  }, []);

  return status;
}
